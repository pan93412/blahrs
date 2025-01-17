//! NB. All structs here that are part of signee must be lexically sorted, as RFC8785.
//! This is tested by `canonical_fields_sorted`.
//! See: https://www.rfc-editor.org/rfc/rfc8785
//! FIXME: `typ` is still always the first field because of `serde`'s implementation.
use std::fmt;
use std::time::SystemTime;

use anyhow::{ensure, Context};
use bitflags::bitflags;
use bitflags_serde_shim::impl_serde_for_bitflags;
use ed25519_dalek::{
    Signature, Signer, SigningKey, VerifyingKey, PUBLIC_KEY_LENGTH, SIGNATURE_LENGTH,
};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const TIMESTAMP_TOLERENCE: u64 = 90;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserKey(#[serde(with = "hex::serde")] pub [u8; PUBLIC_KEY_LENGTH]);

impl fmt::Display for UserKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; PUBLIC_KEY_LENGTH * 2];
        hex::encode_to_slice(self.0, &mut buf).unwrap();
        f.write_str(std::str::from_utf8(&buf).unwrap())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WithSig<T> {
    #[serde(with = "hex::serde")]
    pub sig: [u8; SIGNATURE_LENGTH],
    pub signee: Signee<T>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signee<T> {
    pub nonce: u32,
    pub payload: T,
    pub timestamp: u64,
    pub user: UserKey,
}

fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("after UNIX epoch")
        .as_secs()
}

impl<T: Serialize> WithSig<T> {
    pub fn sign(key: &SigningKey, rng: &mut impl RngCore, payload: T) -> anyhow::Result<Self> {
        let signee = Signee {
            nonce: rng.next_u32(),
            payload,
            timestamp: get_timestamp(),
            user: UserKey(key.verifying_key().to_bytes()),
        };
        let canonical_signee = serde_json::to_vec(&signee).context("failed to serialize")?;
        let sig = key.try_sign(&canonical_signee)?.to_bytes();
        Ok(Self { sig, signee })
    }

    pub fn verify(&self) -> anyhow::Result<()> {
        ensure!(
            self.signee.timestamp.abs_diff(get_timestamp()) < TIMESTAMP_TOLERENCE,
            "invalid timestamp"
        );

        let canonical_signee = serde_json::to_vec(&self.signee).context("failed to serialize")?;
        let sig = Signature::from_bytes(&self.sig);
        VerifyingKey::from_bytes(&self.signee.user.0)?.verify_strict(&canonical_signee, &sig)?;
        Ok(())
    }
}

// FIXME: `deny_unknown_fields` breaks this.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "typ", rename = "chat")]
pub struct ChatPayload {
    pub room: Uuid,
    pub text: String,
}

pub type ChatItem = WithSig<ChatPayload>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "typ", rename = "create_room")]
pub struct CreateRoomPayload {
    pub attrs: RoomAttrs,
    /// The initial member list. Besides invariants of `RoomMemberList`, this also must include the
    /// room creator themselves, with the highest permission (-1).
    pub members: RoomMemberList,
    pub title: String,
}

/// A collection of room members, with these invariants:
/// 1. Sorted by userkeys.
/// 2. No duplicated users.
#[derive(Debug, Deserialize)]
#[serde(try_from = "Vec<RoomMember>")]
pub struct RoomMemberList(pub Vec<RoomMember>);

impl Serialize for RoomMemberList {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(ser)
    }
}

impl TryFrom<Vec<RoomMember>> for RoomMemberList {
    type Error = &'static str;

    fn try_from(members: Vec<RoomMember>) -> Result<Self, Self::Error> {
        if members.windows(2).all(|w| w[0].user.0 < w[1].user.0) {
            Ok(Self(members))
        } else {
            Err("unsorted or duplicated users")
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RoomMember {
    pub permission: MemberPermission,
    pub user: UserKey,
}

/// Proof of room membership for read-access.
///
/// TODO: Should we use JWT here instead?
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "typ", rename = "auth")]
pub struct AuthPayload {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "typ", rename_all = "snake_case")]
pub enum RoomAdminPayload {
    AddMember {
        permission: MemberPermission,
        room: Uuid,
        user: UserKey,
    },
    // TODO: CRUD
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ServerPermission: u64 {
        const CREATE_ROOM = 1 << 0;

        const ALL = !0;
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MemberPermission: u64 {
        const POST_CHAT = 1 << 0;
        const ADD_MEMBER = 1 << 1;

        const ALL = !0;
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct RoomAttrs: u64 {
        const PUBLIC_READABLE = 1 << 0;

        const _ = !0;
    }
}

impl_serde_for_bitflags!(ServerPermission);
impl_serde_for_bitflags!(MemberPermission);
impl_serde_for_bitflags!(RoomAttrs);

mod sql_impl {
    use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
    use rusqlite::{Result, ToSql};

    use super::*;

    impl ToSql for UserKey {
        fn to_sql(&self) -> Result<ToSqlOutput<'_>> {
            // TODO: Extensive key format?
            self.0.to_sql()
        }
    }

    impl FromSql for UserKey {
        fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
            let rawkey = <[u8; PUBLIC_KEY_LENGTH]>::column_result(value)?;
            let key = VerifyingKey::from_bytes(&rawkey)
                .map_err(|err| FromSqlError::Other(format!("invalid pubkey: {err}").into()))?;
            Ok(UserKey(key.to_bytes()))
        }
    }

    macro_rules! impl_u64_flag {
        ($($name:ident),*) => {
            $(
                impl ToSql for $name {
                    fn to_sql(&self) -> Result<ToSqlOutput<'_>> {
                        // Cast out the sign.
                        Ok((self.bits() as i64).into())
                    }
                }

                impl FromSql for $name {
                    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                        // Cast out the sign.
                        i64::column_result(value).map(|v| $name::from_bits_retain(v as u64))
                    }
                }
            )*
        };
    }

    impl_u64_flag!(ServerPermission, MemberPermission, RoomAttrs);
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    #[derive(Default)]
    struct Visitor {
        errors: String,
    }

    impl<'ast> syn::visit::Visit<'ast> for Visitor {
        fn visit_fields_named(&mut self, i: &'ast syn::FieldsNamed) {
            let fields = i
                .named
                .iter()
                .flat_map(|f| f.ident.clone())
                .map(|i| i.to_string())
                .collect::<Vec<_>>();
            if !fields.windows(2).all(|w| w[0] < w[1]) {
                writeln!(self.errors, "unsorted fields: {fields:?}").unwrap();
            }
        }
    }

    #[test]
    fn canonical_fields_sorted() {
        let src = std::fs::read_to_string(file!()).unwrap();
        let file = syn::parse_file(&src).unwrap();

        let mut v = Visitor::default();
        syn::visit::visit_file(&mut v, &file);
        if !v.errors.is_empty() {
            panic!("{}", v.errors);
        }
    }
}
