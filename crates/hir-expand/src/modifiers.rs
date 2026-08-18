//! Declaration modifiers as a fixed set of booleans (the source-side analog
//! of the JVM access flags carried by the classfile stubs).

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub public: bool,
    pub protected: bool,
    pub private: bool,
    pub static_: bool,
    pub final_: bool,
    pub abstract_: bool,
    pub sealed: bool,
    pub non_sealed: bool,
    pub strictfp: bool,
    pub default: bool,
    pub native: bool,
    pub synchronized: bool,
    pub transient: bool,
    pub volatile: bool,
}

impl Modifiers {
    /// Sets the flag for a modifier keyword name. Returns `false` for
    /// unrecognized modifiers (e.g. `transitive`/`open` module modifiers).
    pub fn push(&mut self, keyword: &str) -> bool {
        match keyword {
            "public" => self.public = true,
            "protected" => self.protected = true,
            "private" => self.private = true,
            "static" => self.static_ = true,
            "final" => self.final_ = true,
            "abstract" => self.abstract_ = true,
            "sealed" => self.sealed = true,
            "non-sealed" => self.non_sealed = true,
            "strictfp" => self.strictfp = true,
            "default" => self.default = true,
            "native" => self.native = true,
            "synchronized" => self.synchronized = true,
            "transient" => self.transient = true,
            "volatile" => self.volatile = true,
            _ => return false,
        }
        true
    }

    /// The recognized modifier names, in display order.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        [
            (self.public, "public"),
            (self.protected, "protected"),
            (self.private, "private"),
            (self.static_, "static"),
            (self.final_, "final"),
            (self.abstract_, "abstract"),
            (self.sealed, "sealed"),
            (self.non_sealed, "non-sealed"),
            (self.strictfp, "strictfp"),
            (self.default, "default"),
            (self.native, "native"),
            (self.synchronized, "synchronized"),
            (self.transient, "transient"),
            (self.volatile, "volatile"),
        ]
        .into_iter()
        .filter_map(|(set, name)| set.then_some(name))
    }
}
