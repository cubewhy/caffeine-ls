pub(crate) fn consume_one_descriptor_type(s: &str) -> (&str, &str) {
    match s.chars().next() {
        Some('L') => {
            if let Some(end) = s.find(';') {
                (&s[..=end], &s[end + 1..])
            } else {
                (s, "")
            }
        }
        Some('[') => {
            let (_, rest) = consume_one_descriptor_type(&s[1..]);
            let consumed = s.len() - rest.len();
            (&s[..consumed], rest)
        }
        Some(_) => (&s[..1], &s[1..]),
        None => ("", ""),
    }
}

/// Split method parameter descriptors from a JVM method descriptor.
///
/// "(ILjava/lang/String;[B)V" -> ["I", "Ljava/lang/String;", "[B"]
pub fn split_param_descriptors(descriptor: &str) -> Vec<&str> {
    let (l, r) = match descriptor.find('(').zip(descriptor.find(')')) {
        Some(x) => x,
        None => return vec![],
    };

    let mut out = Vec::new();
    let mut s = &descriptor[l + 1..r];

    while !s.is_empty() {
        let (one, rest) = consume_one_descriptor_type(s);
        if one.is_empty() {
            break;
        }
        out.push(one);
        s = rest;
    }

    out
}
