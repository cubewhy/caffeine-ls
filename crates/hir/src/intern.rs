use smol_str::SmolStr;

#[salsa::interned]
#[derive(Debug)]
pub struct PackageId {
    pub parent: Option<PackageId<'db>>,
    pub name: SmolStr,
}

#[salsa::interned]
#[derive(Debug)]
pub struct ClassId {
    pub package: Option<PackageId<'db>>,
    pub name: SmolStr,
}

#[salsa::interned]
#[derive(Debug)]
pub struct ModuleId {
    pub name: SmolStr,
}

#[salsa::interned]
#[derive(Debug)]
pub struct MethodId {
    pub class: ClassId<'db>,
    pub name: SmolStr,
    pub descriptor: SmolStr,
}

#[salsa::interned]
#[derive(Debug)]
pub struct FieldId {
    pub class: ClassId<'db>,
    pub name: SmolStr,
}

#[salsa::db]
pub trait InternDatabase: salsa::Database {}
