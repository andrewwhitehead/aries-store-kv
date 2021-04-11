use alloc::{borrow::Cow, string::String, vec::Vec};

use zeroize::Zeroize;

use crate::{buffer::WriteBuffer, error::Error};

mod encode;
pub use encode::{JwkEncoder, JwkEncoderMode};

mod ops;
pub use self::ops::{KeyOps, KeyOpsSet};

mod parts;
pub use self::parts::JwkParts;

#[derive(Clone, Debug)]
pub enum Jwk<'a> {
    Encoded(Cow<'a, str>),
    Parts(JwkParts<'a>),
}

impl Jwk<'_> {
    pub fn to_parts(&self) -> Result<JwkParts<'_>, Error> {
        match self {
            Self::Encoded(s) => Ok(
                serde_json::from_str(s.as_ref()).map_err(err_map!("Error deserializing JWK"))?
            ),
            Self::Parts(p) => Ok(*p),
        }
    }

    pub fn as_opt_str(&self) -> Option<&str> {
        match self {
            Self::Encoded(s) => Some(s.as_ref()),
            Self::Parts(_) => None,
        }
    }
}

impl<'a> From<Cow<'a, str>> for Jwk<'a> {
    fn from(jwk: Cow<'a, str>) -> Self {
        Jwk::Encoded(jwk)
    }
}

impl<'a> From<&'a str> for Jwk<'a> {
    fn from(jwk: &'a str) -> Self {
        Jwk::Encoded(Cow::Borrowed(jwk))
    }
}

impl<'a> From<String> for Jwk<'a> {
    fn from(jwk: String) -> Self {
        Jwk::Encoded(Cow::Owned(jwk))
    }
}

impl<'a> From<JwkParts<'a>> for Jwk<'a> {
    fn from(jwk: JwkParts<'a>) -> Self {
        Jwk::Parts(jwk)
    }
}

impl Zeroize for Jwk<'_> {
    fn zeroize(&mut self) {
        match self {
            Self::Encoded(Cow::Owned(s)) => s.zeroize(),
            Self::Encoded(_) => (),
            Self::Parts(..) => (),
        }
    }
}

pub trait ToJwk {
    fn to_jwk_buffer<B: WriteBuffer>(&self, buffer: &mut JwkEncoder<B>) -> Result<(), Error>;

    fn to_jwk_public(&self) -> Result<Jwk<'static>, Error> {
        let mut v = Vec::with_capacity(128);
        let mut buf = JwkEncoder::new(&mut v, JwkEncoderMode::PublicKey)?;
        self.to_jwk_buffer(&mut buf)?;
        buf.finalize()?;
        Ok(Jwk::Encoded(Cow::Owned(String::from_utf8(v).unwrap())))
    }

    fn to_jwk_secret(&self) -> Result<Jwk<'static>, Error> {
        let mut v = Vec::with_capacity(128);
        let mut buf = JwkEncoder::new(&mut v, JwkEncoderMode::SecretKey)?;
        self.to_jwk_buffer(&mut buf)?;
        buf.finalize()?;
        Ok(Jwk::Encoded(Cow::Owned(String::from_utf8(v).unwrap())))
    }
}

pub trait FromJwk: Sized {
    fn from_jwk(jwk: Jwk<'_>) -> Result<Self, Error> {
        let parts = jwk.to_parts()?;
        Self::from_jwk_parts(parts)
    }

    fn from_jwk_parts(jwk: JwkParts<'_>) -> Result<Self, Error>;
}

// pub trait JwkBuilder<'s> {
//     // key type
//     kty: &'a str,
//     // curve type
//     crv: Option<&'a str>,
//     // curve key public y coordinate
//     x: Option<&'a str>,
//     // curve key public y coordinate
//     y: Option<&'a str>,
//     // curve key private key bytes
//     d: Option<&'a str>,
//     // used by symmetric keys like AES
//     k: Option<&'a str>,
// }

// impl<'de> Serialize for JwkParts<'de> {
//     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
//     where
//         S: Serializer,
//     {
//         let ret = serializer.serialize_map(None).unwrap();

//         let add_attr = |name: &str, val: &str| {
//             ret.serialize_key(name);
//             ret.serialize_value(val);
//         };

//         add_attr("kty", self.kty.as_ref());
//         if let Some(attr) = self.crv.as_ref() {
//             add_attr("crv", attr.as_ref());
//             if let Some(attr) = self.x.as_ref() {
//                 add_attr("x", attr.as_ref());
//             }
//             if let Some(attr) = self.y.as_ref() {
//                 add_attr("y", attr.as_ref());
//             }
//             if let Some(attr) = self.d.as_ref() {
//                 add_attr("d", attr.as_ref());
//             }
//         }
//         if let Some(attr) = self.k.as_ref() {
//             add_attr("k", attr.as_ref());
//         }
//         ret.end()
//     }
// }
