use crate::helpers::spawn_app;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[tokio::test]
async fn subscribe_returns_a_200_for_valid_form_data() {
    // Arrange
    let test_app = spawn_app().await;
    // Act
    let body = "name=le%20guin&email=ursula_le_guin%40gmail.com";
    Mock::given(path("/email"))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&test_app.email_server)
        .await;

    let response = test_app.post_subscriptions(body.into()).await;
    // Assert
    assert_eq!(200, response.status().as_u16());

    let saved = sqlx::query!("SELECT email, name from subscriptions")
        .fetch_one(&test_app.db_pool)
        .await
        .expect("Failed to fetch saved subscriptions");

    assert_eq!(saved.email, "ursula_le_guin@gmail.com");
    assert_eq!(saved.name, "le guin");
}

#[tokio::test]
async fn subscribe_persist_the_new_subscriber() {
    let app = spawn_app().await;
    let body = "name=le%20guin&email=ursula_le_guin%40gmail.com";
    Mock::given(path("/email"))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&app.email_server)
        .await;

    app.post_subscriptions(body.into()).await;

    let saved = sqlx::query!("SELECT email, name, status FROM subscriptions")
        .fetch_one(&app.db_pool)
        .await
        .expect("Failed to fetch saved subscriptions");

    assert_eq!(saved.email, "ursula_le_guin@gmail.com");
    assert_eq!(saved.name, "le guin");
    assert_eq!(saved.status, "pending_confirmation");
}

#[tokio::test]
async fn subscribe_returns_a_400_when_data_is_missing() {
    // Arrange
    let test_app = spawn_app().await;
    let test_cases = vec![
        ("name=le%20guin", "missing email"),
        ("mail=ursula_le_guin%40gmail.com", "missing name"),
        ("", "missing both name and email"),
    ];

    for (invalid_body, errors_massage) in test_cases {
        // Act
        let response = test_app.post_subscriptions(invalid_body.into()).await;
        // Assert
        assert_eq!(
            400,
            response.status().as_u16(),
            "The API did not fail with 400 Bad Request when the payload was {}",
            errors_massage
        )
    }
}

#[tokio::test]
async fn subscriber_returns_a_400_when_fields_are_present_but_invalid() {
    let app = spawn_app().await;
    let test_cases = vec![
        ("name=&email=ursula_le_guin%40gmail.com", "empty name"),
        ("name=Ursula&email=", "empty email"),
        ("name=Ursula&email=definitely-not-an-email", "invalid email"),
    ];

    for (body, description) in test_cases {
        let response = app.post_subscriptions(body.into()).await;
        assert_eq!(
            400,
            response.status().as_u16(),
            "The API did not return a 400 Bad Request when the payload was {}",
            description
        );
    }
}

#[tokio::test]
async fn subscribe_sends_a_confirmation_email_for_valid_data() {
    // Arrange
    let app = spawn_app().await;
    let body = "name=le%20guin&email=ursula_le_guin%40gmail.com";

    Mock::given(path("/email"))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&app.email_server)
        .await;

    // Act
    app.post_subscriptions(body.into()).await;

    // Assert
    // Mock asserts on drop
}

#[tokio::test]
async fn subscribe_sends_a_confirmation_email_with_a_link() {
    // Arrange
    let app = spawn_app().await;
    let body = "name=le%20guin&email=ursula_le_guin%40gmail.com";

    Mock::given(path("/email"))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&app.email_server)
        .await;

    // Act
    app.post_subscriptions(body.into()).await;

    // Assert
    let email_request = &app.email_server.received_requests().await.unwrap()[0];
    let confirmation_link = app.get_confirmation_links(&email_request);

    assert_eq!(confirmation_link.html, confirmation_link.plain_text);
}

#[tokio::test]
async fn subscribe_sends_a_second_confirmation() {
    // Arrange
    let app = spawn_app().await;
    let body = "name=le%20guin&email=ursula_le_guin%40gmail.com";

    // Mock Email-Server expect 2 Post Requests
    Mock::given(path("/email"))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&app.email_server)
        .await;

    // First Subscription
    app.post_subscriptions(body.into()).await;
    // Second Subscription
    app.post_subscriptions(body.into()).await;
}

#[tokio::test]
async fn after_second_subscription_there_is_only_one_subscription_token() {
    let app = spawn_app().await;
    let body = "name=le%20guin&email=ursula_le_guin%40gmail.com";
    Mock::given(path("/email"))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&app.email_server)
        .await;

    // First Subscription
    app.post_subscriptions(body.into()).await;

    // Get subscriber ID
    let record = sqlx::query!("SELECT id FROM subscriptions")
        .fetch_one(&app.db_pool)
        .await
        .expect("Failed to fetch saved subscriptions");
    let subscriber_id = record.id;

    // Second Subscription
    app.post_subscriptions(body.into()).await;

    // Get record for subscriber_id in subscription_tokens db
    let record = sqlx::query!(
        "SELECT subscription_token FROM subscription_tokens WHERE subscriber_id=$1",
        subscriber_id
    )
    .fetch_all(&app.db_pool)
    .await
    .expect("Failed to fetch saved subscription_tokens");

    assert_eq!(record.len(), 1);
}

#[tokio::test]
async fn update_subscription_token_after_second_subscription() {
    let app = spawn_app().await;
    let body = "name=le%20guin&email=ursula_le_guin%40gmail.com";
    Mock::given(path("/email"))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&app.email_server)
        .await;

    // First Subscription
    app.post_subscriptions(body.into()).await;

    let record = sqlx::query!("SELECT id FROM subscriptions")
        .fetch_one(&app.db_pool)
        .await
        .expect("Failed to fetch saved subscriptions");
    let subscription_id = record.id;

    let first_subscription_token = sqlx::query!(
        "SELECT subscription_token FROM subscription_tokens WHERE subscriber_id = $1",
        subscription_id
    )
    .fetch_one(&app.db_pool)
    .await
    .expect("Failed to fetch first subscription token")
    .subscription_token;

    // Second Subscription
    app.post_subscriptions(body.into()).await;

    let second_subscription_token = sqlx::query!(
        "SELECT subscription_token FROM subscription_tokens WHERE subscriber_id = $1",
        subscription_id
    )
    .fetch_one(&app.db_pool)
    .await
    .expect("Failed to fetch second subscription token")
    .subscription_token;

    assert_ne!(first_subscription_token, second_subscription_token);
}

#[tokio::test]
async fn second_subscribe_sends_a_confirmation_email_with_a_new_link() {
    // Arrange
    let app = spawn_app().await;
    let body = "name=le%20guin&email=ursula_le_guin%40gmail.com";

    Mock::given(path("/email"))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&app.email_server)
        .await;

    // Act First subscription
    app.post_subscriptions(body.into()).await;

    // Assert
    let first_email_request = &app.email_server.received_requests().await.unwrap()[0];
    let first_confirmation_link = app.get_confirmation_links(&first_email_request);

    // Act second subscription
    app.post_subscriptions(body.into()).await;

    // Assert
    let second_email_request = &app.email_server.received_requests().await.unwrap()[1];
    let second_confirmation_link = app.get_confirmation_links(&second_email_request);
    assert_ne!(
        first_confirmation_link.plain_text,
        second_confirmation_link.plain_text
    );
    assert_ne!(first_confirmation_link.html, second_confirmation_link.html);
}
