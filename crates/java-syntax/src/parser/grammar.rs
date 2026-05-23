mod clauses;
mod compilation_unit;
mod decl;
mod error_recover;
mod expr;
mod member;
mod modifiers;
mod names;
mod stmt;
mod types;

pub use compilation_unit::root;

use crate::{
    EntryPoint, Parser,
    grammar::{
        decl::{
            annotation_type_body, class_body, enum_body, interface_body, module_body, record_body,
        },
        expr::array_initializer,
        stmt::{block, switch_block},
    },
};

pub fn partial(p: &mut Parser, entry: EntryPoint) {
    match entry {
        EntryPoint::Root => root(p),
        EntryPoint::Block => block(p),
        EntryPoint::ClassBody => class_body(p),
        EntryPoint::InterfaceBody => interface_body(p),
        EntryPoint::SwitchBlock => switch_block(p),
        EntryPoint::AnnotationTypeBody => annotation_type_body(p),
        EntryPoint::EnumBody => enum_body(p),
        EntryPoint::RecordBody => record_body(p),
        EntryPoint::ModuleBody => module_body(p),
        EntryPoint::ArrayInitializer => {
            array_initializer(p);
        }
    }
}
