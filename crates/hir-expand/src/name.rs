//! A source identifier. Wrapped so the representation (plain `SmolStr` today,
//! an interned `lasso::Spur` or a salsa-interned struct later) can change
//! without touching every consumer.

use std::fmt;

use smol_str::SmolStr;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(SmolStr);

impl Name {
    pub fn new(text: &str) -> Self {
        Name(SmolStr::new(text))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The last `.`-separated segment: `com.example.Outer.Inner` is `Inner`.
    /// `$` is *not* a separator — source names nest with dots only, and a `$`
    /// is an ordinary identifier character ([JLS
    /// §3.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.8)),
    /// so `com.example.A$B` is simply `A$B`. Library binary names (`Outer$Inner`,
    /// JVMS §4.2) never reach this helper; they are decomposed at the
    /// library/source boundary instead.
    pub fn simple_name(&self) -> &str {
        match self.0.rsplit_once('.') {
            Some((_, simple)) => simple,
            None => self.0.as_str(),
        }
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<&str> for Name {
    fn from(text: &str) -> Self {
        Name::new(text)
    }
}

impl From<String> for Name {
    fn from(text: String) -> Self {
        Name(SmolStr::new(text))
    }
}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_name_splits_dots_only() {
        assert_eq!(Name::new("com.example.Outer.Inner").simple_name(), "Inner");
        // §3.8: `$` is part of the identifier, not a nesting separator.
        assert_eq!(Name::new("com.example.A$B").simple_name(), "A$B");
        assert_eq!(Name::new("com.example.C.m$1").simple_name(), "m$1");
        assert_eq!(Name::new("Foo").simple_name(), "Foo");
        assert_eq!(Name::new("").simple_name(), "");
    }
}
