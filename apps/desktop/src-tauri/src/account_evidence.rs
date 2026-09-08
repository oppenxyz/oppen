//! Concrete read-only account evidence shared by native operator ceremonies.

use oppen_core::guardrail::ActivationEvidence;
use oppen_core::ledger::AuthorizedRoute;
use oppen_hl::InfoClient;
use oppen_hl::types::UserRole;

pub(crate) async fn gather(
    info: &InfoClient,
    route: &AuthorizedRoute,
) -> Result<ActivationEvidence, String> {
    let read_started_at_ms = now_ms();
    let account = route.binding.container;
    let (perps, spot, orders, market, account_role, signer_role) = tokio::try_join!(
        info.clearinghouse_state(account),
        info.spot_clearinghouse_state(account),
        info.frontend_open_orders(account),
        info.meta_and_asset_ctxs(),
        info.user_role(account),
        info.user_role(route.binding.wallet.address),
    )
    .map_err(|error| error.to_string())?;
    if !market.is_aligned() {
        return Err("misaligned activation market evidence".into());
    }
    let approval_user = match (&account_role, route.binding.vault_address) {
        (UserRole::User, None) => account,
        (UserRole::SubAccount { master }, Some(vault)) if vault == account => *master,
        _ => return Err("account role does not identify the reviewed approval user".into()),
    };
    let extra_agents = info
        .extra_agents(approval_user)
        .await
        .map_err(|error| error.to_string())?;
    Ok(ActivationEvidence {
        read_started_at_ms,
        read_completed_at_ms: now_ms(),
        perps,
        spot,
        orders,
        reference_prices: market.reference_pxs(),
        account_role,
        signer_role,
        extra_agents,
    })
}

fn now_ms() -> u64 {
    u64::try_from(oppen_core::ledger::now_ms()).unwrap_or(u64::MAX)
}
