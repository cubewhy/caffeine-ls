use crate::lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageKind {
    Java,
    Kotlin,
    /// A `.kts` script file, parsed with the KLS `script` production instead
    /// of the `kotlinFile` production ([spec: grammar-rule-script]).
    KotlinScript,
    Unknown,
}

impl LanguageKind {
    /// The language of the file at `path`, by its extension ([`crate::lang`]).
    pub fn from_path(path: &str) -> Self {
        lang::for_path(path).unwrap_or(LanguageKind::Unknown)
    }

    /// The lowercase name the snapshot renderers spell this language with. A
    /// `.kts` script is spelled as Kotlin: the two kinds are one language
    /// parsed by two productions.
    pub fn name(self) -> &'static str {
        lang::for_kind(self).map_or("unknown", |lang| lang.name())
    }
}
