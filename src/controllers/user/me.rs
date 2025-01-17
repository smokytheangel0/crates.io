use std::collections::HashMap;

use crate::controllers::prelude::*;

use crate::controllers::helpers::*;
use crate::email;
use crate::util::bad_request;
use crate::util::errors::CargoError;

use crate::models::{CrateOwner, Email, Follow, NewEmail, OwnerKind, User, Version};
use crate::schema::{crate_owners, crates, emails, follows, users, versions};
use crate::views::{EncodableMe, EncodableVersion, OwnedCrate};

/// Handles the `GET /me` route.
pub fn me(req: &mut dyn Request) -> CargoResult<Response> {
    // Changed to getting User information from database because in
    // src/tests/user.rs, when testing put and get on updating email,
    // request seems to be somehow 'cached'. When we try to get a
    // request from the /me route with the just updated user (call
    // this function) the user is the same as the initial GET request
    // and does not seem to get the updated user information from the
    // database
    // This change is not preferable, we'd rather fix the request,
    // perhaps adding `req.mut_extensions().insert(user)` to the
    // update_user route, however this somehow does not seem to work

    let user_id = req.user()?.id;
    let conn = req.db_conn()?;

    let (user, verified, email, verification_sent) = users::table
        .find(user_id)
        .left_join(emails::table)
        .select((
            users::all_columns,
            emails::verified.nullable(),
            emails::email.nullable(),
            emails::token_generated_at.nullable().is_not_null(),
        ))
        .first::<(User, Option<bool>, Option<String>, bool)>(&*conn)?;

    let owned_crates = crate_owners::table
        .inner_join(crates::table)
        .filter(crate_owners::owner_id.eq(user_id))
        .filter(crate_owners::owner_kind.eq(OwnerKind::User as i32))
        .select((crates::id, crates::name, crate_owners::email_notifications))
        .order(crates::name.asc())
        .load(&*conn)?
        .into_iter()
        .map(|(id, name, email_notifications)| OwnedCrate {
            id,
            name,
            email_notifications,
        })
        .collect();

    let verified = verified.unwrap_or(false);
    let verification_sent = verified || verification_sent;
    let user = User { email, ..user };

    Ok(req.json(&EncodableMe {
        user: user.encodable_private(verified, verification_sent),
        owned_crates,
    }))
}

/// Handles the `GET /me/updates` route.
pub fn updates(req: &mut dyn Request) -> CargoResult<Response> {
    use diesel::dsl::any;

    let user = req.user()?;
    let conn = req.db_conn()?;

    let followed_crates = Follow::belonging_to(user).select(follows::crate_id);
    let data = versions::table
        .inner_join(crates::table)
        .left_outer_join(users::table)
        .filter(crates::id.eq(any(followed_crates)))
        .order(versions::created_at.desc())
        .select((
            versions::all_columns,
            crates::name,
            users::all_columns.nullable(),
        ))
        .paginate(&req.query())?
        .load::<(Version, String, Option<User>)>(&*conn)?;

    let more = data.next_page_params().is_some();

    let versions = data
        .into_iter()
        .map(|(version, crate_name, published_by)| version.encodable(&crate_name, published_by))
        .collect();

    #[derive(Serialize)]
    struct R {
        versions: Vec<EncodableVersion>,
        meta: Meta,
    }
    #[derive(Serialize)]
    struct Meta {
        more: bool,
    }
    Ok(req.json(&R {
        versions,
        meta: Meta { more },
    }))
}

/// Handles the `PUT /user/:user_id` route.
pub fn update_user(req: &mut dyn Request) -> CargoResult<Response> {
    use self::emails::user_id;
    use self::users::dsl::{email, gh_login, users};
    use diesel::{insert_into, update};

    let mut body = String::new();
    req.body().read_to_string(&mut body)?;
    let user = req.user()?;
    let name = &req.params()["user_id"];
    let conn = req.db_conn()?;

    // need to check if current user matches user to be updated
    if &user.id.to_string() != name {
        return Err(human("current user does not match requested user"));
    }

    #[derive(Deserialize)]
    struct UserUpdate {
        user: User,
    }

    #[derive(Deserialize)]
    struct User {
        email: Option<String>,
    }

    let user_update: UserUpdate =
        serde_json::from_str(&body).map_err(|_| human("invalid json request"))?;

    if user_update.user.email.is_none() {
        return Err(human("empty email rejected"));
    }

    let user_email = user_update.user.email.unwrap();
    let user_email = user_email.trim();

    if user_email == "" {
        return Err(human("empty email rejected"));
    }

    conn.transaction::<_, Box<dyn CargoError>, _>(|| {
        update(users.filter(gh_login.eq(&user.gh_login)))
            .set(email.eq(user_email))
            .execute(&*conn)?;

        let new_email = NewEmail {
            user_id: user.id,
            email: user_email,
        };

        let token = insert_into(emails::table)
            .values(&new_email)
            .on_conflict(user_id)
            .do_update()
            .set(&new_email)
            .returning(emails::token)
            .get_result::<String>(&*conn)
            .map_err(|_| human("Error in creating token"))?;

        crate::email::send_user_confirm_email(user_email, &user.gh_login, &token);

        Ok(())
    })?;

    #[derive(Serialize)]
    struct R {
        ok: bool,
    }
    Ok(req.json(&R { ok: true }))
}

/// Handles the `PUT /confirm/:email_token` route
pub fn confirm_user_email(req: &mut dyn Request) -> CargoResult<Response> {
    use diesel::update;

    let conn = req.db_conn()?;
    let req_token = &req.params()["email_token"];

    let updated_rows = update(emails::table.filter(emails::token.eq(req_token)))
        .set(emails::verified.eq(true))
        .execute(&*conn)?;

    if updated_rows == 0 {
        return Err(bad_request("Email belonging to token not found."));
    }

    #[derive(Serialize)]
    struct R {
        ok: bool,
    }
    Ok(req.json(&R { ok: true }))
}

/// Handles `PUT /user/:user_id/resend` route
pub fn regenerate_token_and_send(req: &mut dyn Request) -> CargoResult<Response> {
    use diesel::dsl::sql;
    use diesel::update;

    let user = req.user()?;
    let name = &req.params()["user_id"].parse::<i32>().ok().unwrap();
    let conn = req.db_conn()?;

    // need to check if current user matches user to be updated
    if &user.id != name {
        return Err(human("current user does not match requested user"));
    }

    conn.transaction(|| {
        let email = update(Email::belonging_to(user))
            .set(emails::token.eq(sql("DEFAULT")))
            .get_result::<Email>(&*conn)
            .map_err(|_| bad_request("Email could not be found"))?;

        email::try_send_user_confirm_email(&email.email, &user.gh_login, &email.token)
            .map_err(|_| bad_request("Error in sending email"))
    })?;

    #[derive(Serialize)]
    struct R {
        ok: bool,
    }
    Ok(req.json(&R { ok: true }))
}

/// Handles `PUT /me/email_notifications` route
pub fn update_email_notifications(req: &mut dyn Request) -> CargoResult<Response> {
    use self::crate_owners::dsl::*;
    use diesel::pg::upsert::excluded;

    #[derive(Deserialize)]
    struct CrateEmailNotifications {
        id: i32,
        email_notifications: bool,
    }

    let mut body = String::new();
    req.body().read_to_string(&mut body)?;
    let updates: HashMap<i32, bool> = serde_json::from_str::<Vec<CrateEmailNotifications>>(&body)
        .map_err(|_| bad_request("invalid json request"))?
        .iter()
        .map(|c| (c.id, c.email_notifications))
        .collect();

    let user = req.user()?;
    let conn = req.db_conn()?;

    // Build inserts from existing crates beloning to the current user
    let to_insert = CrateOwner::by_owner_kind(OwnerKind::User)
        .filter(owner_id.eq(user.id))
        .select((crate_id, owner_id, owner_kind, email_notifications))
        .load(&*conn)?
        .into_iter()
        // Remove records whose `email_notifications` will not change from their current value
        .map(
            |(c_id, o_id, o_kind, e_notifications): (i32, i32, i32, bool)| {
                let current_e_notifications = *updates.get(&c_id).unwrap_or(&e_notifications);
                (
                    crate_id.eq(c_id),
                    owner_id.eq(o_id),
                    owner_kind.eq(o_kind),
                    email_notifications.eq(current_e_notifications),
                )
            },
        )
        .collect::<Vec<_>>();

    // Upsert crate owners; this should only actually exectute updates
    diesel::insert_into(crate_owners)
        .values(&to_insert)
        .on_conflict((crate_id, owner_id, owner_kind))
        .do_update()
        .set(email_notifications.eq(excluded(email_notifications)))
        .execute(&*conn)?;

    #[derive(Serialize)]
    struct R {
        ok: bool,
    }
    Ok(req.json(&R { ok: true }))
}
