//! A compact classfile writer and jar builder for end-to-end tests.
//!
//! Adapted from `hir`'s integration-test fixture (`hir/tests/common.rs`): the
//! copy there stays where it is — moving it would churn three crates' test
//! targets — while the LSP-level tests need the same raw classfile bytes to
//! build a dependency jar and its `-sources.jar` side by side.

use std::{io::Write as _, path::Path};

use zip::write::{SimpleFileOptions, ZipWriter};

/// Hand-encodes a public class `fqn` (slash-separated, e.g. `com/example/Foo`)
/// extending `java.lang.Object`, with a default constructor, the given
/// `public int` fields and `public void` methods (each `(name, parameter
/// count)` of `int` parameters), as a classfile digestible by `rust-asm`.
///
/// The class carries no `MethodParameters` attribute: that is the whole point
/// of the source merge — a library method's parameter names can then only come
/// from the source declaration.
pub fn class_bytes(fqn: &str, fields: &[&str], methods: &[(&str, usize)]) -> Vec<u8> {
    /// Appends a `CONSTANT_Utf8` entry, returning its constant-pool index.
    fn utf8(entries: &mut Vec<Vec<u8>>, value: &str) -> u16 {
        let mut bytes = vec![1u8];
        bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
        entries.push(bytes);
        entries.len() as u16
    }

    /// Appends a `CONSTANT_Class` entry referring to a `Utf8` entry.
    fn class_ref(entries: &mut Vec<Vec<u8>>, name_index: u16) -> u16 {
        let mut bytes = vec![7u8];
        bytes.extend_from_slice(&name_index.to_be_bytes());
        entries.push(bytes);
        entries.len() as u16
    }

    let mut entries: Vec<Vec<u8>> = Vec::new();
    let fqn_index = utf8(&mut entries, fqn);
    let this_class = class_ref(&mut entries, fqn_index);
    let object_index = utf8(&mut entries, "java/lang/Object");
    let super_class = class_ref(&mut entries, object_index);
    let init_name = utf8(&mut entries, "<init>");
    let init_descriptor = utf8(&mut entries, "()V");

    let field_indices: Vec<(u16, u16)> = fields
        .iter()
        .map(|name| {
            let name = utf8(&mut entries, name);
            let descriptor = utf8(&mut entries, "I");
            (name, descriptor)
        })
        .collect();
    let method_indices: Vec<(u16, u16)> = methods
        .iter()
        .map(|(name, arity)| {
            let descriptor = format!("({})V", "I".repeat(*arity));
            let name = utf8(&mut entries, name);
            let descriptor = utf8(&mut entries, &descriptor);
            (name, descriptor)
        })
        .collect();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    bytes.extend_from_slice(&0u16.to_be_bytes()); // minor version
    bytes.extend_from_slice(&52u16.to_be_bytes()); // major version
    bytes.extend_from_slice(&((entries.len() + 1) as u16).to_be_bytes());
    for entry in &entries {
        bytes.extend_from_slice(entry);
    }

    bytes.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
    bytes.extend_from_slice(&this_class.to_be_bytes());
    bytes.extend_from_slice(&super_class.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes()); // interfaces

    bytes.extend_from_slice(&(field_indices.len() as u16).to_be_bytes());
    for (name, descriptor) in &field_indices {
        bytes.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
        bytes.extend_from_slice(&name.to_be_bytes());
        bytes.extend_from_slice(&descriptor.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes()); // attributes
    }

    let method_count = method_indices.len() + 1;
    bytes.extend_from_slice(&(method_count as u16).to_be_bytes());
    for (name, descriptor) in
        std::iter::once(&(init_name, init_descriptor)).chain(method_indices.iter())
    {
        bytes.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
        bytes.extend_from_slice(&name.to_be_bytes());
        bytes.extend_from_slice(&descriptor.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes()); // attributes
    }

    bytes.extend_from_slice(&0u16.to_be_bytes()); // class attributes
    bytes
}

/// Writes `path` as a zip archive holding each `(entry_name, bytes)` pair.
pub fn build_jar(path: &Path, entries: &[(&str, Vec<u8>)]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    for (name, bytes) in entries {
        zip.start_file(*name, options)?;
        zip.write_all(bytes)?;
    }
    zip.finish()?;
    Ok(())
}
