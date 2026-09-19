//! Process-wide intern pool for [`NodeKind`](crate::NodeKind) and [`LangId`](crate::LangId).

use std::sync::LazyLock;

use lasso::{Spur, ThreadedRodeo};

static STRINGS: LazyLock<ThreadedRodeo> = LazyLock::new(ThreadedRodeo::default);

pub(crate) fn intern(s: &str) -> Spur {
    STRINGS.get_or_intern(s)
}

pub(crate) fn resolve(key: Spur) -> &'static str {
    STRINGS.resolve(&key)
}
