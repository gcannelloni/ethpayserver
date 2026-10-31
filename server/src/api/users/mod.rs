//! User API endpoints: API keys, wallet login credentials, account deletion
//! and email change.
//!
//! All endpoints require authentication via session token.
//!
//! Split by behaviour rather than kept as one file - each submodule covers
//! one of the groupings above, along the same lines as `admin::deletion`.

pub(crate) mod api_keys;
mod deletion;
pub(crate) mod devices;
pub(crate) mod email;
mod key_material;
pub(crate) mod signup;
pub(crate) mod standing;
pub(crate) mod wallet_rotations;
pub(crate) mod wallets;

pub use api_keys::{create_api_key, list_api_keys, revoke_api_key, rotate_api_key, update_api_key};
pub use deletion::{DeleteAccountQuery, delete_account};
pub use devices::{AddDeviceRequest, add_device};
pub use email::{
    ConfirmEmailChangePayload, EmailStatusResponse, RequestEmailChangePayload,
    confirm_email_change, get_email_status, remove_email, request_email_change,
};
pub use signup::{list_plans, signup};
pub use standing::{StandingResponse, get_standing};
pub use wallet_rotations::{WalletRotationEntry, get_wallet_rotations};
pub use wallets::{
    PromoteWalletCredentialRequest, WalletCredentialResponse, WalletReauthChallengeResponse,
    create_wallet_reauth_challenge, list_wallet_credentials, set_primary_wallet_credential,
};

// `ApiKeyInfoResponse` and `CreateApiKeyPayload` now carry `permissions` in
// the pinned `api-types` crate itself, so both come straight from there.
// `CreateApiKeyResponsePayload`/`RotateApiKeyResponsePayload` still don't -
// see `api_keys`'s hand-mirrored versions for why - so those two come from
// this repo instead.
pub use api_keys::{CreateApiKeyResponsePayload, RotateApiKeyResponsePayload};
pub use api_types::{
    ApiKeyInfoResponse, ApiKeyListResponse, CreateApiKeyPayload, UpdateApiKeyPayload,
};
