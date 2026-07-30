use std::iter::Peekable;

use lasso::ThreadedRodeo;
use rust_asm::{
    class_reader::AttributeInfo,
    constant_pool::{ConstantPoolExt, CpInfo},
};
use smol_str::SmolStr;

use crate::ast::{PrimitiveType, TypeParameter, TypeRef};

pub struct SigParser<'a> {
    chars: Peekable<std::str::Chars<'a>>,
    interner: &'a ThreadedRodeo,
}

impl<'a> SigParser<'a> {
    pub fn new(sig: &'a str, interner: &'a ThreadedRodeo) -> Self {
        Self {
            chars: sig.chars().peekable(),
            interner,
        }
    }

    fn consume(&mut self) -> Option<char> {
        self.chars.next()
    }

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    /// Parses: `< T:Ljava/lang/Object; U::Ljava/lang/Runnable; >`
    pub fn parse_type_parameters(&mut self) -> Vec<TypeParameter> {
        let mut params = Vec::new();
        if self.peek() == Some('<') {
            self.consume(); // '<'
            while self.peek() != Some('>') && self.peek().is_some() {
                let mut name = String::new();
                while let Some(c) = self.peek() {
                    if c == ':' || c == '>' {
                        break;
                    }
                    name.push(self.consume().unwrap());
                }
                self.consume(); // ':'

                let mut bounds = Vec::new();
                // If it's not a second ':', we parse the ClassBound
                if self.peek() != Some(':') && self.peek() != Some('>') {
                    bounds.push(self.parse_reference_type_signature());
                }

                // Parse zero or more InterfaceBounds (start with ':')
                while self.peek() == Some(':') {
                    self.consume(); // ':'
                    bounds.push(self.parse_reference_type_signature());
                }
                params.push(TypeParameter {
                    name: self.interner.get_or_intern(name),
                    bounds,
                    annotations: Vec::new(),
                });
            }
            self.consume(); // '>'
        }
        params
    }

    fn parse_type_signature(&mut self) -> TypeRef {
        match self.peek() {
            Some('B') => {
                self.consume();
                TypeRef::Primitive(PrimitiveType::Byte)
            }
            Some('C') => {
                self.consume();
                TypeRef::Primitive(PrimitiveType::Char)
            }
            Some('D') => {
                self.consume();
                TypeRef::Primitive(PrimitiveType::Double)
            }
            Some('F') => {
                self.consume();
                TypeRef::Primitive(PrimitiveType::Float)
            }
            Some('I') => {
                self.consume();
                TypeRef::Primitive(PrimitiveType::Int)
            }
            Some('J') => {
                self.consume();
                TypeRef::Primitive(PrimitiveType::Long)
            }
            Some('S') => {
                self.consume();
                TypeRef::Primitive(PrimitiveType::Short)
            }
            Some('Z') => {
                self.consume();
                TypeRef::Primitive(PrimitiveType::Boolean)
            }
            Some('V') => {
                self.consume();
                TypeRef::Primitive(PrimitiveType::Void)
            }
            Some('[') | Some('T') | Some('L') => self.parse_reference_type_signature(),
            _ => TypeRef::Error,
        }
    }

    pub fn parse_reference_type_signature(&mut self) -> TypeRef {
        match self.peek() {
            Some('T') => {
                self.consume(); // 'T'
                let mut name = String::new();
                while let Some(c) = self.peek() {
                    if c == ';' {
                        self.consume(); // ';'
                        break;
                    }
                    name.push(self.consume().unwrap());
                }
                TypeRef::Reference {
                    name: self.interner.get_or_intern(name),
                    generic_args: Vec::new(),
                }
            }
            Some('L') => {
                self.consume(); // 'L'
                let mut name = String::new();
                let mut generic_args = Vec::new();
                while let Some(c) = self.peek() {
                    if c == ';' {
                        self.consume(); // ';'
                        break;
                    } else if c == '<' {
                        self.consume(); // '<'
                        while self.peek() != Some('>') && self.peek().is_some() {
                            generic_args.push(self.parse_type_argument());
                        }
                        self.consume(); // '>'
                    } else if c == '.' {
                        self.consume(); // '.'
                        name.push('$'); // align with standard JVM nested class naming
                    } else {
                        name.push(self.consume().unwrap());
                    }
                }
                TypeRef::Reference {
                    name: self.interner.get_or_intern(name.replace("/", ".")),
                    generic_args,
                }
            }
            Some('[') => {
                self.consume(); // '['
                TypeRef::Array(Box::new(self.parse_type_signature()))
            }
            _ => TypeRef::Error,
        }
    }

    fn parse_type_argument(&mut self) -> TypeRef {
        match self.peek() {
            Some('*') => {
                self.consume();
                TypeRef::Reference {
                    name: self.interner.get_or_intern_static("?"),
                    generic_args: Vec::new(),
                }
            }
            Some('+') | Some('-') => {
                self.consume(); // Handle Wildcard upper ('+') and lower ('-') bound by passing boundary
                self.parse_reference_type_signature()
            }
            _ => self.parse_reference_type_signature(),
        }
    }

    pub fn parse_class_signature(&mut self) -> (Vec<TypeParameter>, TypeRef, Vec<TypeRef>) {
        let type_params = self.parse_type_parameters();
        let super_class = self.parse_reference_type_signature();
        let mut interfaces = Vec::new();
        while self.peek().is_some() {
            interfaces.push(self.parse_reference_type_signature());
        }
        (type_params, super_class, interfaces)
    }

    pub fn parse_method_signature(
        &mut self,
    ) -> (Vec<TypeParameter>, Vec<TypeRef>, TypeRef, Vec<TypeRef>) {
        let type_params = self.parse_type_parameters();
        let mut param_types = Vec::new();
        if self.peek() == Some('(') {
            self.consume();
            while self.peek() != Some(')') && self.peek().is_some() {
                param_types.push(self.parse_type_signature());
            }
            self.consume(); // ')'
        }
        let return_type = self.parse_type_signature();
        let mut throws = Vec::new();
        while self.peek() == Some('^') {
            self.consume();
            throws.push(self.parse_reference_type_signature());
        }
        (type_params, param_types, return_type, throws)
    }
}

// Helper to extract the JVM Signature string if present
pub fn get_signature(attributes: &[AttributeInfo], cp: &[CpInfo]) -> Option<String> {
    attributes.iter().find_map(|attr| {
        if let AttributeInfo::Signature { signature_index } = attr {
            cp.resolve_utf8(*signature_index).map(|s| s.to_string())
        } else {
            None
        }
    })
}
