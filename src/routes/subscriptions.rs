use crate::domain::{NewSubscriber, SubscriberEmail, SubscriberName};
use crate::email_client::EmailClient;
use crate::routes::error_chain_fmt;
use crate::startup::ApplicationBaseUrl;
use actix_web::http::StatusCode;
use actix_web::{web, HttpResponse, ResponseError};
use anyhow::Context;
use chrono::Utc;
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use sqlx::postgres::PgRow;
use sqlx::{Executor, PgPool, Postgres, Row, Transaction};
use std::fmt::Formatter;
use tera::Tera;
use uuid::Uuid;

#[derive(serde::Deserialize)]
pub struct FormData {
    email: String,
    name: String,
}

impl TryFrom<FormData> for NewSubscriber {
    type Error = String;

    fn try_from(value: FormData) -> Result<Self, Self::Error> {
        let name = SubscriberName::parse(value.name)?;
        let email = SubscriberEmail::parse(value.email)?;
        Ok(NewSubscriber { email, name })
    }
}

#[tracing::instrument(
    name = "Adding a new subscriber",
    skip(form, pool, email_client, base_url),
    fields(
        subscriber_email = %form.email,
        subscriber_name = %form.name
    )
)]
pub async fn subscribe(
    form: web::Form<FormData>,
    pool: web::Data<PgPool>,
    email_client: web::Data<EmailClient>,
    base_url: web::Data<ApplicationBaseUrl>,
) -> Result<HttpResponse, SubscribeError> {
    let new_subscriber = form.0.try_into().map_err(SubscribeError::ValidationError)?;

    let mut transaction = pool
        .begin()
        .await
        .context("Failed to acquire a Postgres connection from the pool")?;

    let subscriber_id = insert_subscriber(&mut transaction, &new_subscriber)
        .await
        .context("Failed to insert new subscriber in the database.")?;

    let subscription_token = generate_subscription_token();

    store_token(&mut transaction, subscriber_id, &subscription_token)
        .await
        .context("Failed to store the confirmation token for a new subscriber")?;

    transaction
        .commit()
        .await
        .context("Failed to commit SQL transaction to store a new subscriber")?;

    send_confirmation_email(
        &email_client,
        new_subscriber,
        &base_url.0,
        &subscription_token,
    )
    .await
    .context("Failed to send a confirmation email")?;

    Ok(HttpResponse::Ok().finish())
}

#[tracing::instrument(
    name= "Send a confirmation email to a new subscriber"
    skip(email_client, new_subscriber, base_url)
)]
pub async fn send_confirmation_email(
    email_client: &EmailClient,
    new_subscriber: NewSubscriber,
    base_url: &str,
    subscription_token: &str,
) -> Result<(), reqwest::Error> {
    // Email
    let confirmation_link = format!(
        "{}/subscriptions/confirm?subscription_token={}",
        base_url, subscription_token,
    );
    let plain_body = &format!(
        "Welcome to our newsletter!\nVisit {} to confirm your subscription.",
        confirmation_link
    );
    let html_body = generate_html_form(new_subscriber.name.as_ref(), &confirmation_link);

    email_client
        .send_email(&new_subscriber.email, "Welcome!", &html_body, plain_body)
        .await
}

#[tracing::instrument(
    name = "Saving new subscriber in the database",
    skip(new_subscriber, transaction)
)]
pub async fn insert_subscriber(
    transaction: &mut Transaction<'_, Postgres>,
    new_subscriber: &NewSubscriber,
) -> Result<Uuid, sqlx::Error> {
    // Check for existing subscriber
    let existing_subscriber = check_for_existing_subscriber(transaction, new_subscriber).await?;

    // Act if subscriber exist already
    // If User already exists return the Uuid from this user
    if let Some(record) = existing_subscriber {
        tracing::info!(
            "Subscriber with email {} already exists",
            new_subscriber.email.as_ref()
        );
        return Ok(record.get("id"));
    }

    // Else create new Uuid for subscriber an add the subscriber to the database
    let subscriber_id = Uuid::new_v4();
    let query = sqlx::query!(
        r#"INSERT INTO subscriptions (id, email, name, subscribed_at, status) VALUES ($1, $2, $3, $4, 'pending_confirmation')"#,
        subscriber_id,
        new_subscriber.email.as_ref(),
        new_subscriber.name.as_ref(),
        Utc::now()
    );

    transaction.execute(query).await?;
    Ok(subscriber_id)
}

async fn check_for_existing_subscriber(
    transaction: &mut Transaction<'_, Postgres>,
    new_subscriber: &NewSubscriber,
) -> Result<Option<PgRow>, sqlx::Error> {
    // Check if subscriber is already in the database
    let query = sqlx::query!(
        r#"Select id FROM subscriptions WHERE email = $1"#,
        new_subscriber.email.as_ref()
    );

    let record = transaction.fetch_optional(query).await.map_err(|e| {
        tracing::error!("Failed to check for existing subscriber: {:?}", e);
        e
    })?;

    Ok(record)
}

#[tracing::instrument(
    name = "Store subscription token in the database",
    skip(subscription_token, transaction)
)]
pub async fn store_token(
    transaction: &mut Transaction<'_, Postgres>,
    subscriber_id: Uuid,
    subscription_token: &str,
) -> Result<(), StoreTokenError> {
    // Check if for the user a subscription_token already exist
    let existing_token = check_for_existing_token(transaction, subscriber_id).await?;

    if existing_token.is_some() {
        tracing::info!(
            "Subscription token for subscriber: {:?} already exists",
            subscriber_id
        );
        let query = sqlx::query!(
            r#"UPDATE subscription_tokens SET subscription_token = $1 WHERE subscriber_id = $2"#,
            subscription_token,
            subscriber_id
        );
        transaction.execute(query).await.map_err(StoreTokenError)?;
    } else {
        let query = sqlx::query!(
            r#"INSERT INTO subscription_tokens (subscription_token, subscriber_id) VALUES ($1, $2)"#,
            subscription_token,
            subscriber_id
        );
        transaction.execute(query).await.map_err(|e| {
            tracing::error!("Failed to insert subscription_token: {:?}", e);
            StoreTokenError(e)
        })?;
    }
    Ok(())
}

async fn check_for_existing_token(
    transaction: &mut Transaction<'_, Postgres>,
    subscriber_id: Uuid,
) -> Result<Option<PgRow>, StoreTokenError> {
    // Check if subscriber is already in the database
    let query = sqlx::query!(
        r#"SELECT subscription_token FROM subscription_tokens WHERE subscriber_id = $1"#,
        subscriber_id
    );

    let record = transaction.fetch_optional(query).await.map_err(|e| {
        tracing::error!("Failed to check for existing token: {:?}", e);
        StoreTokenError(e)
    })?;

    Ok(record)
}

fn generate_subscription_token() -> String {
    let mut rng = thread_rng();
    std::iter::repeat_with(|| rng.sample(Alphanumeric))
        .map(char::from)
        .take(25)
        .collect()
}

fn generate_html_form(subscriber_name: &str, confirmation_link: &str) -> String {
    let tera = Tera::new("templates/**/*").unwrap();
    let mut context = tera::Context::new();
    context.insert("confirmation_link", confirmation_link);
    context.insert("name", subscriber_name);
    tera.render("hello_email.html", &context).unwrap()
}

// A new error type, wrapping s sqlx::Error
pub struct StoreTokenError(sqlx::Error);

impl std::fmt::Debug for StoreTokenError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        error_chain_fmt(self, f)
    }
}
impl std::fmt::Display for StoreTokenError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            fmt,
            "A database error was encountered while trying to store a subscription token"
        )
    }
}

impl std::error::Error for StoreTokenError {
    fn cause(&self) -> Option<&dyn std::error::Error> {
        Some(&self.0)
    }
}

#[derive(thiserror::Error)]
pub enum SubscribeError {
    #[error("{0}")]
    ValidationError(String),
    #[error(transparent)]
    UnexpectedError(#[from] anyhow::Error),
}

impl std::fmt::Debug for SubscribeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        error_chain_fmt(self, f)
    }
}

impl ResponseError for SubscribeError {
    fn status_code(&self) -> StatusCode {
        match self {
            SubscribeError::ValidationError(_) => StatusCode::BAD_REQUEST,
            SubscribeError::UnexpectedError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
