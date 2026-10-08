//! Lower-case hexadecimal, the way digests are written in stored records
//! and keys.

/// `bytes` as lower-case hexadecimal, two digits a byte.
pub fn lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

#[cfg(test)]
mod tests {
    #[test]
    fn bytes_become_two_lower_case_digits_each() {
        assert_eq!(super::lower(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
        assert_eq!(super::lower(&[]), "");
    }
}
