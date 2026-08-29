//! Fully qualified names ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7))
//! on the JVM substrate.
//!
//! A fully qualified name is the `.`-joined dotted name of a package or type
//! ([JLS §6.7]), distinct from a JVM *binary* name ([JVMS §4.2]) which joins
//! nested types with `$`. [`FqName`] is a thin newtype over [`hir_expand::name::Name`]
//! (which already models dotted source names) giving the substrate a typed
//! API for names that are known to be fully qualified.

use hir_expand::name::Name;

/// A fully qualified name: a dot-joined package or type name ([JLS §6.7]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FqName(Name);

impl FqName {
    /// Wraps a name that is already known to be fully qualified.
    pub fn new(name: Name) -> Self {
        FqName(name)
    }

    /// Builds a fully qualified name from a dotted text form.
    pub fn from_str(text: &str) -> Self {
        FqName(Name::new(text))
    }

    /// The underlying name.
    pub fn as_name(&self) -> &Name {
        &self.0
    }

    /// The underlying dotted text.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The package prefix of the name — everything before the last `.` — or
    /// `None` for a name without dots (a bare simple name or the unnamed
    /// package).
    pub fn package(&self) -> Option<&str> {
        match self.0.as_str().rsplit_once('.') {
            Some((package, _)) => Some(package),
            None => None,
        }
    }

    /// The last `.`-separated segment of the name ([JLS §6.7]).
    pub fn simple_name(&self) -> &str {
        self.0.simple_name()
    }

    /// Appends a `.`-separated segment, yielding the enclosing-name of a
    /// nested type or member ([JLS §6.7]).
    pub fn join(&self, segment: &str) -> FqName {
        let mut text = String::with_capacity(self.0.as_str().len() + 1 + segment.len());
        text.push_str(self.0.as_str());
        text.push('.');
        text.push_str(segment);
        FqName::from_str(&text)
    }
}

impl std::fmt::Display for FqName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl From<Name> for FqName {
    fn from(name: Name) -> Self {
        FqName(name)
    }
}

impl From<&str> for FqName {
    fn from(text: &str) -> Self {
        FqName::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments() {
        let fqn = FqName::from_str("com.example.Outer.Inner");
        assert_eq!(fqn.simple_name(), "Inner");
        assert_eq!(fqn.package(), Some("com.example.Outer"));
        assert_eq!(
            fqn.join("member").as_str(),
            "com.example.Outer.Inner.member"
        );
        assert_eq!(FqName::from_str("A").package(), None);
        assert_eq!(FqName::from_str("A").simple_name(), "A");
    }
}
