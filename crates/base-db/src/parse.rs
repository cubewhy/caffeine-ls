use vfs::FileId;

use crate::{LanguageKind, SourceDatabase};
use syntax::Parse;

#[salsa::tracked(returns(ref))]
fn parse_query(db: &dyn SourceDatabase, file_id: FileId, language: LanguageKind) -> Parse {
    let text = db.file_text(file_id).text(db);
    syntax::SourceFile::parse(language, text)
}

pub fn parse(db: &dyn SourceDatabase, file_id: FileId, language: LanguageKind) -> &Parse {
    parse_query(db, file_id, language)
}
