use axum::{
    extract::{self, Query},
    routing::{get, post},
    Extension, Json, Router,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::db::billing_subscription::StripeSubscriptionStatus;
use crate::db::BillingSubscriptionId;
use crate::{AppState, Error, Result};

pub fn router() -> Router {
    Router::new()
        .route(
            "/billing/preferences",
            get(get_billing_preferences).put(update_billing_preferences),
        )
        .route(
            "/billing/subscriptions",
            get(list_billing_subscriptions).post(create_billing_subscription),
        )
        .route(
            "/billing/subscriptions/manage",
            post(manage_billing_subscription),
        )
        .route("/billing/monthly_spend", get(get_monthly_spend))
}

#[derive(Debug, Deserialize)]
struct GetBillingPreferencesParams {
    github_user_id: i32,
}

#[derive(Debug, Serialize)]
struct BillingPreferencesResponse {
    max_monthly_llm_usage_spending_in_cents: i32,
}

async fn get_billing_preferences(
    Extension(_): Extension<Arc<AppState>>,
    Query(_): Query<GetBillingPreferencesParams>,
) -> Result<Json<BillingPreferencesResponse>> {
    Err(Error::http(
        StatusCode::NOT_IMPLEMENTED,
        "not supported".into(),
    ))?
}

#[derive(Debug, Deserialize)]
struct UpdateBillingPreferencesBody {
    github_user_id: i32,
    max_monthly_llm_usage_spending_in_cents: i32,
}

async fn update_billing_preferences(
    Extension(_): Extension<Arc<AppState>>,
    Extension(_): Extension<Arc<crate::rpc::Server>>,
    extract::Json(_): extract::Json<UpdateBillingPreferencesBody>,
) -> Result<Json<BillingPreferencesResponse>> {
    Err(Error::http(
        StatusCode::NOT_IMPLEMENTED,
        "not supported".into(),
    ))?
}

#[derive(Debug, Deserialize)]
struct ListBillingSubscriptionsParams {
    github_user_id: i32,
}

#[derive(Debug, Serialize)]
struct BillingSubscriptionJson {
    id: BillingSubscriptionId,
    name: String,
    status: StripeSubscriptionStatus,
    cancel_at: Option<String>,
    /// Whether this subscription can be canceled.
    is_cancelable: bool,
}

#[derive(Debug, Serialize)]
struct ListBillingSubscriptionsResponse {
    subscriptions: Vec<BillingSubscriptionJson>,
}

async fn list_billing_subscriptions(
    Extension(_): Extension<Arc<AppState>>,
    Query(_): Query<ListBillingSubscriptionsParams>,
) -> Result<Json<ListBillingSubscriptionsResponse>> {
    Err(Error::http(
        StatusCode::NOT_IMPLEMENTED,
        "not supported".into(),
    ))?
}

#[derive(Debug, Deserialize)]
struct CreateBillingSubscriptionBody {
    github_user_id: i32,
}

#[derive(Debug, Serialize)]
struct CreateBillingSubscriptionResponse {
    checkout_session_url: String,
}

/// Initiates a Stripe Checkout session for creating a billing subscription.
async fn create_billing_subscription(
    Extension(_): Extension<Arc<AppState>>,
    extract::Json(_): extract::Json<CreateBillingSubscriptionBody>,
) -> Result<Json<CreateBillingSubscriptionResponse>> {
    Err(Error::http(
        StatusCode::NOT_IMPLEMENTED,
        "not supported".into(),
    ))?
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManageSubscriptionIntent {
    /// The user intends to cancel their subscription.
    Cancel,
    /// The user intends to stop the cancellation of their subscription.
    StopCancellation,
}

#[derive(Debug, Deserialize)]
struct ManageBillingSubscriptionBody {
    github_user_id: i32,
    intent: ManageSubscriptionIntent,
    /// The ID of the subscription to manage.
    subscription_id: BillingSubscriptionId,
}

#[derive(Debug, Serialize)]
struct ManageBillingSubscriptionResponse {
    billing_portal_session_url: Option<String>,
}

/// Initiates a Stripe customer portal session for managing a billing subscription.
async fn manage_billing_subscription(
    Extension(_): Extension<Arc<AppState>>,
    extract::Json(_): extract::Json<ManageBillingSubscriptionBody>,
) -> Result<Json<ManageBillingSubscriptionResponse>> {
    Err(Error::http(
        StatusCode::NOT_IMPLEMENTED,
        "not supported".into(),
    ))?
}

#[derive(Debug, Deserialize)]
struct GetMonthlySpendParams {
    github_user_id: i32,
}

#[derive(Debug, Serialize)]
struct GetMonthlySpendResponse {
    monthly_free_tier_spend_in_cents: u32,
    monthly_free_tier_allowance_in_cents: u32,
    monthly_spend_in_cents: u32,
}

async fn get_monthly_spend(
    Extension(_): Extension<Arc<AppState>>,
    Query(_): Query<GetMonthlySpendParams>,
) -> Result<Json<GetMonthlySpendResponse>> {
    Err(Error::http(
        StatusCode::NOT_IMPLEMENTED,
        "not supported".into(),
    ))?
}
