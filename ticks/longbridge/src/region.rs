//! Longbridge's two networks, mainland China and global. A login picks one through the
//! authorization server it is made with, and the API is then reached through the same network.

/// The authorization server of the mainland China network, as the package's manifest lists it.
pub const CHINA_MAINLAND_ISSUER: &str = "https://openapi.longbridge.cn";
/// The authorization server of the global network, as the package's manifest lists it.
pub const GLOBAL_ISSUER: &str = "https://openapi.longbridge.com";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Cn,
    Global,
}

impl Region {
    /// The network of the authorization server `issuer`: mainland China for a `.cn` host, else
    /// global.
    pub fn of_issuer(issuer: &str) -> Self {
        let is_china_mainland = reqwest::Url::parse(issuer)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.ends_with(".cn")))
            .unwrap_or(false);
        if is_china_mainland {
            Self::Cn
        } else {
            Self::Global
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cn => "cn",
            Self::Global => "global",
        }
    }
}

/// The authorization server a login of each network is made with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issuers {
    pub china_mainland: String,
    pub global: String,
}

impl Issuers {
    pub fn of(&self, region: Region) -> &str {
        match region {
            Region::Cn => &self.china_mainland,
            Region::Global => &self.global,
        }
    }
}

impl Default for Issuers {
    /// Longbridge's own servers.
    fn default() -> Self {
        Self {
            china_mainland: CHINA_MAINLAND_ISSUER.to_owned(),
            global: GLOBAL_ISSUER.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_reaches_the_network_of_its_authorization_server() {
        assert_eq!(Region::of_issuer(CHINA_MAINLAND_ISSUER), Region::Cn);
        assert_eq!(Region::of_issuer(GLOBAL_ISSUER), Region::Global);
        assert_eq!(Region::of_issuer("http://127.0.0.1:8080"), Region::Global);
        assert_eq!(Region::of_issuer("not a url"), Region::Global);
        let issuers = Issuers::default();
        assert_eq!(issuers.of(Region::Cn), CHINA_MAINLAND_ISSUER);
        assert_eq!(issuers.of(Region::Global), GLOBAL_ISSUER);
    }
}
