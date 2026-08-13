//! Stage 0 — small Rust exercises that pin down the ownership ideas the
//! rest of the engine depends on.
//!
//! These are not the database. They are the language mechanics I want to be
//! able to explain before writing a memtable: moving a `String`, borrowing it
//! as `&str`, handing a `Vec<u8>` as `&[u8]`, returning `Option<&T>`,
//! propagating errors with `?`, and matching enum variants.

/// Takes ownership of a `String`. After this call the caller can no longer
/// use the original value — that is a *move*.
fn take_owned(name: String) -> usize {
    name.len()
}

/// Borrows a `String` as `&str`. The caller keeps the `String`.
#[allow(clippy::ptr_arg)]
fn borrow_as_str(name: &String) -> &str {
    name.as_str()
}

/// Accepts a borrowed byte slice. A `Vec<u8>` can be passed as `&[u8]`
/// without giving up ownership.
fn inspect_bytes(bytes: &[u8]) -> usize {
    bytes.len()
}

/// Returns a borrowed reference into `items` when the index is in range.
fn get_item(items: &[u32], index: usize) -> Option<&u32> {
    items.get(index)
}

/// A tiny error used to practice `?`.
#[derive(Debug, PartialEq, Eq)]
enum ParseError {
    Empty,
    NotADigit,
}

fn parse_digit(input: &str) -> Result<u32, ParseError> {
    let first = input.chars().next().ok_or(ParseError::Empty)?;
    first.to_digit(10).ok_or(ParseError::NotADigit)
}

fn parse_then_double(input: &str) -> Result<u32, ParseError> {
    let digit = parse_digit(input)?;
    Ok(digit * 2)
}

/// A key/value pair with an `impl` block — the shape every later struct uses.
struct Pair {
    key: Vec<u8>,
    value: Vec<u8>,
}

impl Pair {
    fn new(key: Vec<u8>, value: Vec<u8>) -> Self {
        Self { key, value }
    }

    fn key(&self) -> &[u8] {
        &self.key
    }

    fn value(&self) -> &[u8] {
        &self.value
    }
}

enum Status {
    Ready,
    Busy(String),
}

fn describe(status: &Status) -> String {
    match status {
        Status::Ready => "ready".to_string(),
        Status::Busy(task) => format!("busy: {task}"),
    }
}

#[test]
fn moving_a_string_transfers_ownership() {
    let name = String::from("Srijan");
    let len = take_owned(name);
    assert_eq!(len, 6);
    // `name` cannot be used here; it was moved into `take_owned`.
}

#[test]
fn borrowing_a_string_as_str_leaves_the_owner_intact() {
    let name = String::from("Srijan");
    let borrowed = borrow_as_str(&name);
    assert_eq!(borrowed, "Srijan");
    assert_eq!(name, "Srijan");
}

#[test]
fn a_vec_can_be_passed_as_a_byte_slice() {
    let bytes = b"rust".to_vec();
    assert_eq!(inspect_bytes(&bytes), 4);
    assert_eq!(bytes, b"rust");
}

#[test]
fn get_item_returns_option_ref() {
    let items = vec![10, 20, 30];
    assert_eq!(get_item(&items, 1), Some(&20));
    assert_eq!(get_item(&items, 9), None);
}

#[test]
fn question_mark_propagates_parse_errors() {
    assert_eq!(parse_then_double("7"), Ok(14));
    assert_eq!(parse_then_double(""), Err(ParseError::Empty));
    assert_eq!(parse_then_double("x"), Err(ParseError::NotADigit));
}

#[test]
fn matching_enum_variants() {
    assert_eq!(describe(&Status::Ready), "ready");
    assert_eq!(describe(&Status::Busy("flush".into())), "busy: flush");
}

#[test]
fn struct_with_impl_block() {
    let pair = Pair::new(b"name".to_vec(), b"Srijan".to_vec());
    assert_eq!(pair.key(), b"name");
    assert_eq!(pair.value(), b"Srijan");
}

/// `fn store(key: Vec<u8>)` takes ownership: the caller loses the vector.
/// `fn find(key: &[u8])` only borrows: the caller keeps it.
#[test]
fn store_takes_ownership_find_only_borrows() {
    fn store(key: Vec<u8>) -> usize {
        key.len()
    }
    fn find(key: &[u8]) -> usize {
        key.len()
    }

    let owned = b"name".to_vec();
    assert_eq!(find(&owned), 4);
    assert_eq!(store(owned), 4);
}
