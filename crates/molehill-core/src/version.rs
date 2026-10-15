//! Loose dotted-version comparison ("2.7.8", "v2.6.23", "2.7.8-I001").

pub fn parse(v: &str) -> Vec<u32> {
    v.trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+', ' '])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0))
        .collect()
}

/// True when `candidate` is strictly newer than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (mut a, mut b) = (parse(candidate), parse(current));
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a > b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares() {
        assert!(is_newer("2.7.8", "2.6.23"));
        assert!(is_newer("v2.7.10", "2.7.9"));
        assert!(!is_newer("2.7.8", "2.7.8-I001"));
        assert!(!is_newer("2.7", "2.7.0"));
    }
}
