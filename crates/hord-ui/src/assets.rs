//! Static assets, embedded in the binary (spec §10.4: one binary, no
//! frontend build pipeline).

/// An embedded file served under `static/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asset {
    /// File name under `static/`.
    pub name: &'static str,
    /// `Content-Type`.
    pub content_type: &'static str,
    /// Contents.
    pub body: &'static str,
}

/// Every static asset.
pub const ASSETS: &[Asset] = &[
    Asset {
        name: "hord.css",
        content_type: "text/css; charset=utf-8",
        body: include_str!("../static/hord.css"),
    },
    Asset {
        name: "strip.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("../static/strip.js"),
    },
    Asset {
        name: "playback.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("../static/playback.js"),
    },
];

/// The asset named `name`, if any.
#[must_use]
pub fn asset(name: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|a| a.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_the_templates_link_is_embedded() {
        for name in ["hord.css", "strip.js", "playback.js"] {
            assert!(asset(name).is_some_and(|a| !a.body.is_empty()), "{name}");
        }
        assert!(asset("../Cargo.toml").is_none());
    }
}
