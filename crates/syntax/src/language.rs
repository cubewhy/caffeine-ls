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
    pub fn from_path(path: &str) -> Self {
        if path.ends_with(".java") {
            LanguageKind::Java
        } else if path.ends_with(".kts") {
            LanguageKind::KotlinScript
        } else if path.ends_with(".kt") {
            LanguageKind::Kotlin
        } else {
            LanguageKind::Unknown
        }
    }
}
