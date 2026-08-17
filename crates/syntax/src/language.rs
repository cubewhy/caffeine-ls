#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageKind {
    Java,
    Kotlin,
    Unknown,
}

impl LanguageKind {
    pub fn from_path(path: &str) -> Self {
        if path.ends_with(".java") {
            LanguageKind::Java
        } else if path.ends_with(".kt") || path.ends_with(".kts") {
            LanguageKind::Kotlin
        } else {
            LanguageKind::Unknown
        }
    }
}
