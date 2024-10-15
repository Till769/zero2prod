use crate::domain::{NewSubscriber, SubscriberEmail, SubscriberName};
use crate::email_client::EmailClient;
use crate::startup::ApplicationBaseUrl;
use actix_web::{web, HttpResponse};
use chrono::Utc;
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use sqlx::postgres::PgRow;
use sqlx::{Executor, PgPool, Postgres, Row, Transaction};
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
) -> HttpResponse {
    let new_subscriber = match form.0.try_into() {
        Ok(subscriber) => subscriber,
        Err(e) => {
            return HttpResponse::BadRequest().body(format!("Failed to parse form data: {}", e));
        }
    };

    let mut transaction = match pool.begin().await {
        Ok(transaction) => transaction,
        Err(e) => {
            tracing::error!("Failed to start a database transaction: {:?}", e);
            return HttpResponse::InternalServerError()
                .body("Failed to start a database transaction.");
        }
    };

    let subscriber_id = match insert_subscriber(&mut transaction, &new_subscriber).await {
        Ok(subscriber_id) => subscriber_id,
        Err(e) => {
            tracing::error!("Failed to insert new subscriber into the database: {:?}", e);
            return HttpResponse::InternalServerError()
                .body("Failed to insert new subscriber into the database.");
        }
    };
    let subscription_token = generate_subscription_token();
    if store_token(&mut transaction, subscriber_id, &subscription_token)
        .await
        .is_err()
    {
        return HttpResponse::InternalServerError().body("Failed to store token");
    }

    if transaction.commit().await.is_err() {
        return HttpResponse::InternalServerError().body("Failed to commit transaction");
    }

    if send_confirmation_email(
        &email_client,
        new_subscriber,
        &base_url.0,
        &subscription_token,
    )
    .await
    .is_err()
    {
        return HttpResponse::InternalServerError().body("Failed to send email");
    }

    HttpResponse::Ok().finish()
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
    let html_body = &format!(
        "„Welcome to our newsletter!<br />\
                Click <a href=\"{}\">here</a> to confirm your subscription.",
        confirmation_link
    );

    email_client
        .send_email(&new_subscriber.email, "Welcome!", html_body, plain_body)
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

    transaction.execute(query).await.map_err(|e| {
        tracing::error!("Failed to execute query: {:?}", e);
        e
    })?;
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
) -> Result<(), sqlx::Error> {
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
        transaction.execute(query).await.map_err(|e| {
            tracing::error!("Failed to update subscription_token: {:?}", e);
            e
        })?;
    } else {
        let query = sqlx::query!(
            r#"INSERT INTO subscription_tokens (subscription_token, subscriber_id) VALUES ($1, $2)"#,
            subscription_token,
            subscriber_id
        );
        transaction.execute(query).await.map_err(|e| {
            tracing::error!("Failed to insert subscription_token: {:?}", e);
            e
        })?;
    }
    Ok(())
}

async fn check_for_existing_token(
    transaction: &mut Transaction<'_, Postgres>,
    subscriber_id: Uuid,
) -> Result<Option<PgRow>, sqlx::Error> {
    // Check if subscriber is already in the database
    let query = sqlx::query!(
        r#"SELECT subscription_token FROM subscription_tokens WHERE subscriber_id = $1"#,
        subscriber_id
    );

    let record = transaction.fetch_optional(query).await.map_err(|e| {
        tracing::error!("Failed to check for existing token: {:?}", e);
        e
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
