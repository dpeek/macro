use std::collections::HashSet;

use sqlx::{Pool, Postgres};

#[cfg(test)]
mod test;

/// Creates a new user in the database and attaches provided roles
///
/// `macro_user_customer_id` supplies the required legacy account key. Only an
/// actual Stripe customer belongs in the optional profile billing field.
#[expect(clippy::too_many_arguments, reason = "too annoying to fix")]
#[tracing::instrument(skip(db))]
pub async fn create_user(
    db: &Pool<Postgres>,
    id: &str,
    username: &str,
    email: &str,
    is_verified: bool,
    macro_user_customer_id: &str,
    stripe_customer_id: Option<&str>,
    organization_id: Option<i32>,
    roles: HashSet<String>,
) -> anyhow::Result<String> {
    let id = macro_uuid::string_to_uuid(id)?;

    let mut transaction = db.begin().await?;

    // Create macro user
    let macro_user_id = sqlx::query!(
        r#"
        INSERT INTO "macro_user" ("id", "username", "stripe_customer_id", "email", "has_trialed")
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id
        "#,
        &id,
        username,
        macro_user_customer_id,
        email,
        false // has_trialed
    )
    .map(|row| row.id)
    .fetch_one(&mut *transaction)
    .await?;

    sqlx::query!(
        r#"
        INSERT INTO "macro_user_email_verification" ("macro_user_id", "email", "is_verified")
        VALUES ($1, $2, $3)
        "#,
        &id,
        email,
        is_verified
    )
    .execute(&mut *transaction)
    .await?;

    // Create user profile
    let id = format!("macro|{}", email);

    let user_id = sqlx::query!(
        r#"
        INSERT INTO "User" ("id", "email", "stripeCustomerId", "organizationId", "macro_user_id")
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id
        "#,
        &id,
        email,
        stripe_customer_id,
        organization_id,
        &macro_user_id
    )
    .map(|row| row.id)
    .fetch_one(&mut *transaction)
    .await?;

    for role in roles {
        sqlx::query!(
            r#"
            INSERT INTO "RolesOnUsers" ("userId", "roleId")
            VALUES ($1, $2)
        "#,
            user_id,
            role
        )
        .execute(&mut *transaction)
        .await?;
    }

    transaction.commit().await?;

    Ok(id)
}

/// Creates a new user profile in the database and attaches provided roles
#[tracing::instrument(skip(db))]
pub async fn create_user_profile(
    db: &Pool<Postgres>,
    id: &str,
    email: &str,
    organization_id: Option<i32>,
    roles: HashSet<String>,
) -> anyhow::Result<()> {
    let macro_user_id = macro_uuid::string_to_uuid(id)?;
    let user_profile_id = format!("macro|{}", email);

    let mut transaction = db.begin().await?;

    let user_id = sqlx::query!(
        r#"
        INSERT INTO "User" ("id", "email", "organizationId", "macro_user_id")
        VALUES ($1, $2, $3, $4)
        RETURNING id
        "#,
        &user_profile_id,
        email,
        organization_id,
        &macro_user_id
    )
    .map(|row| row.id)
    .fetch_one(&mut *transaction)
    .await?;

    for role in roles {
        sqlx::query!(
            r#"
            INSERT INTO "RolesOnUsers" ("userId", "roleId")
            VALUES ($1, $2)
        "#,
            user_id,
            role
        )
        .execute(&mut *transaction)
        .await?;
    }

    transaction.commit().await?;

    Ok(())
}

/// Completes Fog provisioning after the auth service has verified the user's
/// Stalwart identity and mailbox. The profile email must still match the grant.
#[tracing::instrument(skip(db), err)]
pub async fn provision_verified_stalwart_user(
    db: &Pool<Postgres>,
    fusionauth_user_id: &str,
    email: &str,
) -> anyhow::Result<()> {
    let fusionauth_user_id = macro_uuid::string_to_uuid(fusionauth_user_id)?;
    let mut transaction = db.begin().await?;
    let profile = sqlx::query_scalar!(
        r#"
        SELECT u.id FROM "User" u
        JOIN macro_user m ON m.id = u.macro_user_id
        WHERE m.id = $1 AND LOWER(u.email) = LOWER($2)
        FOR UPDATE OF u
        "#,
        fusionauth_user_id,
        email,
    )
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| anyhow::anyhow!("verified Stalwart user has no matching Macro profile"))?;

    sqlx::query!(
        r#"INSERT INTO "RolesOnUsers" ("userId", "roleId")
           VALUES ($1, 'corporate') ON CONFLICT DO NOTHING"#,
        profile,
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query!(
        r#"DELETE FROM "RolesOnUsers" WHERE "userId" = $1 AND "roleId" = 'self_serve'"#,
        profile,
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query!(
        r#"UPDATE "User" SET "tutorialComplete" = TRUE WHERE id = $1"#,
        profile,
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query!(
        r#"
        INSERT INTO user_onboarding (user_id, status, skipped, completed_at)
        VALUES ($1, 'completed', TRUE, NOW())
        ON CONFLICT (user_id) DO UPDATE SET
            status = 'completed', skipped = TRUE,
            completed_at = COALESCE(user_onboarding.completed_at, EXCLUDED.completed_at),
            updated_at = NOW()
        "#,
        profile,
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn stalwart_provisioning_requires_matching_profile_and_is_idempotent(db: Pool<Postgres>) {
        let user_id = "ca289963-0ab8-4a05-bbb3-148c855c75e1";
        let email = "david@drumkicks.com";
        create_user(
            &db,
            user_id,
            "david",
            email,
            true,
            "stripe-stalwart-provisioning-test",
            None,
            ["self_serve".to_string()].into_iter().collect(),
        )
        .await
        .unwrap();

        assert!(
            provision_verified_stalwart_user(&db, user_id, "other@drumkicks.com")
                .await
                .is_err()
        );
        let profile = format!("macro|{email}");
        let roles_before = sqlx::query_scalar!(
            r#"SELECT "roleId" FROM "RolesOnUsers" WHERE "userId" = $1"#,
            profile,
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(roles_before, vec!["self_serve"]);

        provision_verified_stalwart_user(&db, user_id, "David@DrumKicks.com")
            .await
            .unwrap();
        provision_verified_stalwart_user(&db, user_id, email)
            .await
            .unwrap();
        let roles_after = sqlx::query_scalar!(
            r#"SELECT "roleId" FROM "RolesOnUsers" WHERE "userId" = $1"#,
            profile,
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(roles_after, vec!["corporate"]);
        let state = sqlx::query!(
            r#"
            SELECT u."tutorialComplete" AS tutorial_complete,
                   o.status, o.skipped, o.completed_at
            FROM "User" u JOIN user_onboarding o ON o.user_id = u.id
            WHERE u.id = $1
            "#,
            profile,
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(state.tutorial_complete);
        assert_eq!(state.status, "completed");
        assert!(state.skipped);
        assert!(state.completed_at.is_some());
    }
}
