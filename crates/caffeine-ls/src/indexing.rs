use std::{fs::File, io::Read, path::Path};

use jimage_rs::JImage;
use lasso::ThreadedRodeo;
use syntax::{ClassOrModuleStub, ClassStub, class_parser::ClassParser};

pub(crate) fn parse_jar(path: &Path, interner: &ThreadedRodeo) -> Vec<ClassStub> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return Vec::new();
    };
    let parser = ClassParser::new(interner);
    let mut stubs = Vec::new();
    for index in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(index) else {
            continue;
        };
        if !entry.name().ends_with(".class") || entry.name() == "module-info.class" {
            continue;
        }
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_err() {
            continue;
        }
        if let Ok(ClassOrModuleStub::Class(stub)) = parser.parse_cafebabe(&bytes) {
            stubs.push(stub);
        }
    }
    stubs
}

pub(crate) fn parse_jdk(home: &Path, interner: &ThreadedRodeo) -> Vec<ClassStub> {
    let modules = home.join("lib").join("modules");
    if !modules.exists() {
        return parse_jar(&home.join("lib").join("rt.jar"), interner);
    }

    let Ok(image) = JImage::open(&modules) else {
        return Vec::new();
    };
    let Ok(names) = image.resource_names() else {
        return Vec::new();
    };
    let parser = ClassParser::new(interner);
    names
        .into_iter()
        .filter_map(|resource| {
            let (module, path) = resource.get_full_name();
            if !path.ends_with(".class") || path == "module-info.class" {
                return None;
            }
            let key = format!("/{module}/{path}");
            let bytes = image.find_resource(&key).ok().flatten()?;
            match parser.parse_cafebabe(&bytes).ok()? {
                ClassOrModuleStub::Class(stub) => Some(stub),
                ClassOrModuleStub::Module(_) => None,
            }
        })
        .collect()
}
