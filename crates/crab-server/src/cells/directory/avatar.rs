use super::*;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) enum AvatarVariant {
    #[default]
    Beam,
    Marble,
    Pixel,
    Sunset,
    Ring,
    Bauhaus,
    Geometric,
    Abstract,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct AvatarConfig {
    pub variant: AvatarVariant,
    pub seed: String,
    pub colors: Vec<String>,
}

impl AvatarConfig {
    pub(crate) fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.seed.is_empty()
            || self.seed.trim() != self.seed
            || self.seed.chars().count() > 100
            || self.seed.chars().any(char::is_control)
        {
            return Err(
                "Avatar name must contain 1–100 characters without surrounding whitespace or controls",
            );
        }
        if self.colors.is_empty() || self.colors.len() > 6 {
            return Err("Avatar palette must contain between 1 and 6 colors");
        }
        if !self.colors.iter().all(|color| is_hex_color(color)) {
            return Err("Avatar colors must be 6-digit hex values such as #92A1C6");
        }
        Ok(())
    }
}

fn is_hex_color(color: &str) -> bool {
    let bytes = color.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(u8::is_ascii_hexdigit)
}

fn key(actor: Uuid) -> String {
    format!("directory/v1/avatar/{actor}")
}

pub(super) fn load(context: &Transaction<'_>, actor: Uuid) -> Result<Option<AvatarConfig>> {
    context.document(&key(actor))
}

pub(crate) struct GetOwnAvatar;
impl Read for GetOwnAvatar {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 34;
    type Input = IdentityKey;
    type Output = Option<AvatarConfig>;

    fn execute(context: &mut Transaction<'_>, identity: IdentityKey) -> Result<Self::Output> {
        let actor = LookupAccounts::execute(context, vec![identity])?
            .into_iter()
            .next()
            .flatten();
        match actor {
            Some(actor) => load(context, actor),
            None => Ok(None),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetAvatarInput {
    pub identity: IdentityKey,
    pub avatar: Option<AvatarConfig>,
}

pub(crate) struct SetOwnAvatar;
impl Mutation for SetOwnAvatar {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 34;
    type Input = SetAvatarInput;
    type Output = Option<AvatarConfig>;

    fn execute(
        context: &mut Transaction<'_>,
        input: SetAvatarInput,
    ) -> Result<MutationResult<Self::Output>> {
        input.identity.validate()?;
        if let Some(ref avatar) = input.avatar {
            avatar.validate().map_err(Error::Invalid)?;
        }
        let (actor, _) = accounts::ensure(context, &input.identity)?;
        let doc_key = key(actor);
        match input.avatar {
            Some(ref avatar) => context.put_document(&doc_key, avatar)?,
            None => {
                let _ = context.delete_document(&doc_key)?;
            }
        }
        advance(context)?;
        Ok(MutationResult::Success(input.avatar))
    }
}
