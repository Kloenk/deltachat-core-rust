//! # Ephemeral messages
//!
//! Ephemeral messages are messages that have an Ephemeral-Timer
//! header attached to them, which specifies time in seconds after
//! which the message should be deleted both from the device and from
//! the server. The timer is started when the message is marked as
//! seen, which usually happens when its contents is displayed on
//! device screen.
//!
//! Each chat, including 1:1, group chats and "saved messages" chat,
//! has its own ephemeral timer setting, which is applied to all
//! messages sent to the chat. The setting is synchronized to all the
//! devices participating in the chat by applying the timer value from
//! all received messages, including BCC-self ones, to the chat. This
//! way the setting is eventually synchronized among all participants.
//!
//! When user changes ephemeral timer setting for the chat, a system
//! message is automatically sent to update the setting for all
//! participants. This allows changing the setting for a chat like any
//! group chat setting, e.g. name and avatar, without the need to
//! write an actual message.
//!
//! ## Device settings
//!
//! In addition to per-chat ephemeral message setting, each device has
//! two global user-configured settings that complement per-chat
//! settings: `delete_device_after` and `delete_server_after`. These
//! settings are not synchronized among devices and apply to all
//! messages known to the device, including messages sent or received
//! before configuring the setting.
//!
//! `delete_device_after` configures the maximum time device is
//! storing the messages locally. `delete_server_after` configures the
//! time after which device will delete the messages it knows about
//! from the server.
//!
//! ## How messages are deleted
//!
//! When the message is deleted locally, its contents is removed and
//! it is moved to the trash chat. This database entry is then used to
//! track the Message-ID and corresponding IMAP folder and UID until
//! the message is deleted from the server. Vice versa, when device
//! deletes the message from the server, it removes IMAP folder and
//! UID information, but keeps the message contents. When database
//! entry is both moved to trash chat and does not contain UID
//! information, it is deleted from the database, leaving no trace of
//! the message.
//!
//! ## When messages are deleted
//!
//! Local deletion happens when the chatlist or chat is loaded. A
//! `MsgsChanged` event is emitted when a message deletion is due, to
//! make UI reload displayed messages and cause actual deletion.
//!
//! Server deletion happens by generating IMAP deletion jobs based on
//! the database entries which are expired either according to their
//! ephemeral message timers or global `delete_server_after` setting.

use std::convert::{TryFrom, TryInto};
use std::num::ParseIntError;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context as _, Error};
use async_std::task;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::constants::{
    Viewtype, DC_CHAT_ID_LAST_SPECIAL, DC_CHAT_ID_TRASH, DC_CONTACT_ID_DEVICE, DC_CONTACT_ID_SELF,
};
use crate::context::Context;
use crate::dc_tools::time;
use crate::events::EventType;
use crate::message::{Message, MessageState, MsgId};
use crate::mimeparser::SystemMessage;
use crate::sql;
use crate::stock_str;
use crate::{
    chat::{lookup_by_contact_id, send_msg, ChatId},
    job,
};

#[derive(Debug, PartialEq, Eq, Copy, Clone, Serialize, Deserialize)]
pub enum Timer {
    Disabled,
    Enabled { duration: u32 },
}

impl Timer {
    pub fn to_u32(self) -> u32 {
        match self {
            Self::Disabled => 0,
            Self::Enabled { duration } => duration,
        }
    }

    pub fn from_u32(duration: u32) -> Self {
        if duration == 0 {
            Self::Disabled
        } else {
            Self::Enabled { duration }
        }
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::Disabled
    }
}

impl ToString for Timer {
    fn to_string(&self) -> String {
        self.to_u32().to_string()
    }
}

impl FromStr for Timer {
    type Err = ParseIntError;

    fn from_str(input: &str) -> Result<Timer, ParseIntError> {
        input.parse::<u32>().map(Self::from_u32)
    }
}

impl sqlx::Type<sqlx::Sqlite> for Timer {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <i64 as sqlx::Type<_>>::type_info()
    }

    fn compatible(ty: &sqlx::sqlite::SqliteTypeInfo) -> bool {
        <i64 as sqlx::Type<_>>::compatible(ty)
    }
}

impl<'q> sqlx::Encode<'q, sqlx::Sqlite> for Timer {
    fn encode_by_ref(
        &self,
        args: &mut Vec<sqlx::sqlite::SqliteArgumentValue<'q>>,
    ) -> sqlx::encode::IsNull {
        args.push(sqlx::sqlite::SqliteArgumentValue::Int64(
            self.to_u32() as i64
        ));

        sqlx::encode::IsNull::No
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for Timer {
    fn decode(value: sqlx::sqlite::SqliteValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        let value: i64 = sqlx::Decode::decode(value)?;
        if value == 0 {
            Ok(Self::Disabled)
        } else if let Ok(duration) = u32::try_from(value) {
            Ok(Self::Enabled { duration })
        } else {
            Err(Box::new(sqlx::Error::Decode(Box::new(
                crate::error::OutOfRangeError,
            ))))
        }
    }
}

impl ChatId {
    /// Get ephemeral message timer value in seconds.
    pub async fn get_ephemeral_timer(self, context: &Context) -> Result<Timer, Error> {
        let timer = context
            .sql
            .query_get_value(
                sqlx::query("SELECT ephemeral_timer FROM chats WHERE id=?;").bind(self),
            )
            .await?;
        Ok(timer.unwrap_or_default())
    }

    /// Set ephemeral timer value without sending a message.
    ///
    /// Used when a message arrives indicating that someone else has
    /// changed the timer value for a chat.
    pub(crate) async fn inner_set_ephemeral_timer(
        self,
        context: &Context,
        timer: Timer,
    ) -> Result<(), Error> {
        ensure!(!self.is_special(), "Invalid chat ID");

        context
            .sql
            .execute(
                sqlx::query(
                    "UPDATE chats
             SET ephemeral_timer=?
             WHERE id=?;",
                )
                .bind(timer)
                .bind(self),
            )
            .await?;

        context.emit_event(EventType::ChatEphemeralTimerModified {
            chat_id: self,
            timer,
        });
        Ok(())
    }

    /// Set ephemeral message timer value in seconds.
    ///
    /// If timer value is 0, disable ephemeral message timer.
    pub async fn set_ephemeral_timer(self, context: &Context, timer: Timer) -> Result<(), Error> {
        if timer == self.get_ephemeral_timer(context).await? {
            return Ok(());
        }
        self.inner_set_ephemeral_timer(context, timer).await?;
        let mut msg = Message::new(Viewtype::Text);
        msg.text = Some(stock_ephemeral_timer_changed(context, timer, DC_CONTACT_ID_SELF).await);
        msg.param.set_cmd(SystemMessage::EphemeralTimerChanged);
        if let Err(err) = send_msg(context, self, &mut msg).await {
            error!(
                context,
                "Failed to send a message about ephemeral message timer change: {:?}", err
            );
        }
        Ok(())
    }
}

/// Returns a stock message saying that ephemeral timer is changed to `timer` by `from_id`.
pub(crate) async fn stock_ephemeral_timer_changed(
    context: &Context,
    timer: Timer,
    from_id: u32,
) -> String {
    match timer {
        Timer::Disabled => stock_str::msg_ephemeral_timer_disabled(context, from_id as u32).await,
        Timer::Enabled { duration } => match duration {
            0..=59 => {
                stock_str::msg_ephemeral_timer_enabled(context, timer.to_string(), from_id as u32)
                    .await
            }
            60 => stock_str::msg_ephemeral_timer_minute(context, from_id as u32).await,
            61..=3599 => {
                stock_str::msg_ephemeral_timer_minutes(
                    context,
                    format!("{}", (f64::from(duration) / 6.0).round() / 10.0),
                    from_id as u32,
                )
                .await
            }
            3600 => stock_str::msg_ephemeral_timer_hour(context, from_id as u32).await,
            3601..=86399 => {
                stock_str::msg_ephemeral_timer_hours(
                    context,
                    format!("{}", (f64::from(duration) / 360.0).round() / 10.0),
                    from_id as u32,
                )
                .await
            }
            86400 => stock_str::msg_ephemeral_timer_day(context, from_id as u32).await,
            86401..=604_799 => {
                stock_str::msg_ephemeral_timer_days(
                    context,
                    format!("{}", (f64::from(duration) / 8640.0).round() / 10.0),
                    from_id as u32,
                )
                .await
            }
            604_800 => stock_str::msg_ephemeral_timer_week(context, from_id as u32).await,
            _ => {
                stock_str::msg_ephemeral_timer_weeks(
                    context,
                    format!("{}", (f64::from(duration) / 60480.0).round() / 10.0),
                    from_id as u32,
                )
                .await
            }
        },
    }
}

impl MsgId {
    /// Returns ephemeral message timer value for the message.
    pub(crate) async fn ephemeral_timer(self, context: &Context) -> anyhow::Result<Timer> {
        let res = match context
            .sql
            .query_get_value::<_, i64>(
                sqlx::query("SELECT ephemeral_timer FROM msgs WHERE id=?").bind(self),
            )
            .await?
        {
            None | Some(0) => Timer::Disabled,
            Some(duration) => Timer::Enabled {
                duration: u32::try_from(duration)?,
            },
        };
        Ok(res)
    }

    /// Starts ephemeral message timer for the message if it is not started yet.
    pub(crate) async fn start_ephemeral_timer(self, context: &Context) -> anyhow::Result<()> {
        if let Timer::Enabled { duration } = self.ephemeral_timer(context).await? {
            let ephemeral_timestamp = time() + i64::from(duration);

            context
                .sql
                .execute(
                    sqlx::query(
                        "UPDATE msgs SET ephemeral_timestamp = ? \
                WHERE (ephemeral_timestamp == 0 OR ephemeral_timestamp > ?) \
                AND id = ?",
                    )
                    .bind(ephemeral_timestamp)
                    .bind(ephemeral_timestamp)
                    .bind(self),
                )
                .await?;
            schedule_ephemeral_task(context).await;
        }
        Ok(())
    }
}

/// Deletes messages which are expired according to
/// `delete_device_after` setting or `ephemeral_timestamp` column.
///
/// Returns true if any message is deleted, so caller can emit
/// MsgsChanged event. If nothing has been deleted, returns
/// false. This function does not emit the MsgsChanged event itself,
/// because it is also called when chatlist is reloaded, and emitting
/// MsgsChanged there will cause infinite reload loop.
pub(crate) async fn delete_expired_messages(context: &Context) -> Result<bool, Error> {
    let mut updated = context
        .sql
        .execute(
            sqlx::query(
                // If you change which information is removed here, also change MsgId::trash() and
                // which information dc_receive_imf::add_parts() still adds to the db if the chat_id is TRASH
                r#"
UPDATE msgs
SET 
  chat_id=?, txt='', subject='', txt_raw='', 
  mime_headers='', from_id=0, to_id=0, param=''
WHERE
  ephemeral_timestamp != 0
  AND ephemeral_timestamp <= ?
  AND chat_id != ?
"#,
            )
            .bind(DC_CHAT_ID_TRASH)
            .bind(time())
            .bind(DC_CHAT_ID_TRASH),
        )
        .await
        .context("update failed")?
        > 0;

    if let Some(delete_device_after) = context.get_config_delete_device_after().await? {
        let self_chat_id = lookup_by_contact_id(context, DC_CONTACT_ID_SELF)
            .await
            .unwrap_or_default()
            .0;
        let device_chat_id = lookup_by_contact_id(context, DC_CONTACT_ID_DEVICE)
            .await
            .unwrap_or_default()
            .0;

        let threshold_timestamp = time() - delete_device_after;

        // Delete expired messages
        //
        // Only update the rows that have to be updated, to avoid emitting
        // unnecessary "chat modified" events.
        let rows_modified = context
            .sql
            .execute(
                sqlx::query(
                    "UPDATE msgs \
             SET txt = 'DELETED', chat_id = ? \
             WHERE timestamp < ? \
             AND chat_id > ? \
             AND chat_id != ? \
             AND chat_id != ?",
                )
                .bind(DC_CHAT_ID_TRASH)
                .bind(threshold_timestamp)
                .bind(DC_CHAT_ID_LAST_SPECIAL)
                .bind(self_chat_id)
                .bind(device_chat_id),
            )
            .await
            .context("deleted update failed")?;

        updated |= rows_modified > 0;
    }

    schedule_ephemeral_task(context).await;
    Ok(updated)
}

/// Schedule a task to emit MsgsChanged event when the next local
/// deletion happens. Existing task is cancelled to make sure at most
/// one such task is scheduled at a time.
///
/// UI is expected to reload the chatlist or the chat in response to
/// MsgsChanged event, this will trigger actual deletion.
///
/// This takes into account only per-chat timeouts, because global device
/// timeouts are at least one hour long and deletion is triggered often enough
/// by user actions.
pub async fn schedule_ephemeral_task(context: &Context) {
    let ephemeral_timestamp: Option<i64> = match context
        .sql
        .query_get_value(
            sqlx::query(
                r#"
    SELECT ephemeral_timestamp
    FROM msgs
    WHERE ephemeral_timestamp != 0
      AND chat_id != ?
    ORDER BY ephemeral_timestamp ASC
    LIMIT 1;
    "#,
            )
            .bind(DC_CHAT_ID_TRASH), // Trash contains already deleted messages, skip them
        )
        .await
    {
        Err(err) => {
            warn!(context, "Can't calculate next ephemeral timeout: {}", err);
            return;
        }
        Ok(ephemeral_timestamp) => ephemeral_timestamp,
    };

    // Cancel existing task, if any
    if let Some(ephemeral_task) = context.ephemeral_task.write().await.take() {
        ephemeral_task.cancel().await;
    }

    if let Some(ephemeral_timestamp) = ephemeral_timestamp {
        let now = SystemTime::now();
        let until = UNIX_EPOCH
            + Duration::from_secs(ephemeral_timestamp.try_into().unwrap_or(u64::MAX))
            + Duration::from_secs(1);

        if let Ok(duration) = until.duration_since(now) {
            // Schedule a task, ephemeral_timestamp is in the future
            let context1 = context.clone();
            let ephemeral_task = task::spawn(async move {
                async_std::task::sleep(duration).await;
                emit_event!(
                    context1,
                    EventType::MsgsChanged {
                        chat_id: ChatId::new(0),
                        msg_id: MsgId::new(0)
                    }
                );
            });
            *context.ephemeral_task.write().await = Some(ephemeral_task);
        } else {
            // Emit event immediately
            emit_event!(
                context,
                EventType::MsgsChanged {
                    chat_id: ChatId::new(0),
                    msg_id: MsgId::new(0)
                }
            );
        }
    }
}

/// Returns ID of any expired message that should be deleted from the server.
///
/// It looks up the trash chat too, to find messages that are already
/// deleted locally, but not deleted on the server.
pub(crate) async fn load_imap_deletion_msgid(context: &Context) -> sql::Result<Option<MsgId>> {
    let now = time();

    let threshold_timestamp = match context.get_config_delete_server_after().await? {
        None => 0,
        Some(delete_server_after) => now - delete_server_after,
    };

    let row = context
        .sql
        .fetch_optional(
            sqlx::query(
                "SELECT id FROM msgs \
         WHERE ( \
         timestamp < ? \
         OR (ephemeral_timestamp != 0 AND ephemeral_timestamp <= ?) \
         ) \
         AND server_uid != 0 \
         AND NOT id IN (SELECT foreign_id FROM jobs WHERE action = ?)
         LIMIT 1",
            )
            .bind(threshold_timestamp)
            .bind(now)
            .bind(job::Action::DeleteMsgOnImap),
        )
        .await?;

    if let Some(row) = row {
        let msg_id = row.try_get(0)?;
        Ok(Some(msg_id))
    } else {
        Ok(None)
    }
}

/// Start ephemeral timers for seen messages if they are not started
/// yet.
///
/// It is possible that timers are not started due to a missing or
/// failed `MsgId.start_ephemeral_timer()` call, either in the current
/// or previous version of Delta Chat.
///
/// This function is supposed to be called in the background,
/// e.g. from housekeeping task.
pub(crate) async fn start_ephemeral_timers(context: &Context) -> sql::Result<()> {
    context
        .sql
        .execute(
            sqlx::query(
                "UPDATE msgs \
    SET ephemeral_timestamp = ? + ephemeral_timer \
    WHERE ephemeral_timer > 0 \
    AND ephemeral_timestamp = 0 \
    AND state NOT IN (?, ?, ?)",
            )
            .bind(time())
            .bind(MessageState::InFresh)
            .bind(MessageState::InNoticed)
            .bind(MessageState::OutDraft),
        )
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::param::Params;
    use async_std::task::sleep;

    use super::*;
    use crate::test_utils::TestContext;
    use crate::{
        chat::{self, Chat, ChatItem},
        dc_tools::IsNoneOrEmpty,
    };

    #[async_std::test]
    async fn test_stock_ephemeral_messages() {
        let context = TestContext::new().await;

        assert_eq!(
            stock_ephemeral_timer_changed(&context, Timer::Disabled, DC_CONTACT_ID_SELF).await,
            "Message deletion timer is disabled by me."
        );

        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled { duration: 1 },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 1 s by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled { duration: 30 },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 30 s by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled { duration: 60 },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 1 minute by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled { duration: 90 },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 1.5 minutes by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled { duration: 30 * 60 },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 30 minutes by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled { duration: 60 * 60 },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 1 hour by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled { duration: 5400 },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 1.5 hours by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled {
                    duration: 2 * 60 * 60
                },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 2 hours by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled {
                    duration: 24 * 60 * 60
                },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 1 day by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled {
                    duration: 2 * 24 * 60 * 60
                },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 2 days by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled {
                    duration: 7 * 24 * 60 * 60
                },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 1 week by me."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled {
                    duration: 4 * 7 * 24 * 60 * 60
                },
                DC_CONTACT_ID_SELF
            )
            .await,
            "Message deletion timer is set to 4 weeks by me."
        );
    }

    #[async_std::test]
    async fn test_ephemeral_timer() -> anyhow::Result<()> {
        let alice = TestContext::new_alice().await;
        let bob = TestContext::new_bob().await;

        let chat_alice = alice.create_chat(&bob).await.id;
        let chat_bob = bob.create_chat(&alice).await.id;

        // Alice sends message to Bob
        let mut msg = Message::new(Viewtype::Text);
        chat::prepare_msg(&alice.ctx, chat_alice, &mut msg).await?;
        chat::send_msg(&alice.ctx, chat_alice, &mut msg).await?;
        let sent = alice.pop_sent_msg().await;
        bob.recv_msg(&sent).await;

        // Alice sends second message to Bob, with no timer
        let mut msg = Message::new(Viewtype::Text);
        chat::prepare_msg(&alice.ctx, chat_alice, &mut msg).await?;
        chat::send_msg(&alice.ctx, chat_alice, &mut msg).await?;
        let sent = alice.pop_sent_msg().await;

        assert_eq!(
            chat_bob.get_ephemeral_timer(&bob.ctx).await?,
            Timer::Disabled
        );

        // Bob sets ephemeral timer and sends a message about timer change
        chat_bob
            .set_ephemeral_timer(&bob.ctx, Timer::Enabled { duration: 60 })
            .await?;
        let sent_timer_change = bob.pop_sent_msg().await;

        assert_eq!(
            chat_bob.get_ephemeral_timer(&bob.ctx).await?,
            Timer::Enabled { duration: 60 }
        );

        // Bob receives message from Alice.
        // Alice message has no timer. However, Bob should not disable timer,
        // because Alice replies to old message.
        bob.recv_msg(&sent).await;

        assert_eq!(
            chat_alice.get_ephemeral_timer(&alice.ctx).await?,
            Timer::Disabled
        );
        assert_eq!(
            chat_bob.get_ephemeral_timer(&bob.ctx).await?,
            Timer::Enabled { duration: 60 }
        );

        // Alice receives message from Bob
        alice.recv_msg(&sent_timer_change).await;

        assert_eq!(
            chat_alice.get_ephemeral_timer(&alice.ctx).await?,
            Timer::Enabled { duration: 60 }
        );

        Ok(())
    }

    #[async_std::test]
    async fn test_ephemeral_delete_msgs() {
        let t = TestContext::new_alice().await;
        let chat = t.get_self_chat().await;

        t.send_text(chat.id, "Saved message, which we delete manually")
            .await;
        let msg = t.get_last_msg_in(chat.id).await;
        msg.id.delete_from_db(&t).await.unwrap();
        check_msg_was_deleted(&t, &chat, msg.id).await;

        chat.id
            .set_ephemeral_timer(&t, Timer::Enabled { duration: 1 })
            .await
            .unwrap();
        let msg = t
            .send_text(chat.id, "Saved message, disappearing after 1s")
            .await;

        sleep(Duration::from_millis(1100)).await;

        // Check checks that the msg was deleted locally
        check_msg_was_deleted(&t, &chat, msg.sender_msg_id).await;

        // Check that the msg will be deleted on the server
        // First of all, set a server_uid so that DC thinks that it's actually possible to delete
        t.sql
            .execute(sqlx::query("UPDATE msgs SET server_uid=1 WHERE id=?").bind(msg.sender_msg_id))
            .await
            .unwrap();
        let job = job::load_imap_deletion_job(&t).await.unwrap();
        assert_eq!(
            job,
            Some(job::Job::new(
                job::Action::DeleteMsgOnImap,
                msg.sender_msg_id.to_u32(),
                Params::new(),
                0,
            ))
        );
        // Let's assume that executing the job fails on first try and the job is saved to the db
        job.unwrap().save(&t).await.unwrap();

        // Make sure that we don't get yet another job when loading from db
        let job2 = job::load_imap_deletion_job(&t).await.unwrap();
        assert_eq!(job2, None);
    }

    async fn check_msg_was_deleted(t: &TestContext, chat: &Chat, msg_id: MsgId) {
        let chat_items = chat::get_chat_msgs(t, chat.id, 0, None).await.unwrap();
        // Check that the chat is empty except for possibly info messages:
        for item in &chat_items {
            if let ChatItem::Message { msg_id } = item {
                let msg = Message::load_from_db(t, *msg_id).await.unwrap();
                assert!(msg.is_info())
            }
        }

        // Check that if there is a message left, the text and metadata are gone
        if let Ok(msg) = Message::load_from_db(t, msg_id).await {
            assert_eq!(msg.from_id, 0);
            assert_eq!(msg.to_id, 0);
            assert!(msg.text.is_none_or_empty(), "{:?}", msg.text);
            let rawtxt: Option<String> = t
                .sql
                .query_get_value(sqlx::query("SELECT txt_raw FROM msgs WHERE id=?;").bind(msg_id))
                .await
                .unwrap();
            assert!(rawtxt.is_none_or_empty(), "{:?}", rawtxt);
        }
    }
}
