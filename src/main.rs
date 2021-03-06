use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use log::{debug, info, LevelFilter};
use serenity::async_trait;
use serenity::client::{Client, Context, EventHandler};
use serenity::framework::standard::{
    help_commands,
    macros::{check, command, group, help, hook},
    Args, CommandGroup, CommandOptions, CommandResult, HelpOptions, Reason, StandardFramework,
};
use serenity::http::Http;
use serenity::model::channel::Message;
use serenity::model::prelude::*;
use serenity::static_assertions::_core::sync::atomic::AtomicBool;
use serenity::utils::MessageBuilder;
use simplelog::SimpleLogger;
use sysinfo::get_current_pid;
use tokio::time::Duration;

use commands::dump_messages::DUMP_MESSAGES_COMMAND;
use commands::init_channel::INIT_CHANNEL_COMMAND;
use commands::server_summary::SERVER_SUMMARY_COMMAND;
use commands::show_channels::SHOW_CHANNELS_COMMAND;
use commands::show_stats::SHOW_STATS_COMMAND;
use commands::word_cloud::GEN_WORDCLOUD_COMMAND;
use commands::feedback::FEEDBACK_COMMAND;

use crate::config::{GeneralAppConfig, GeneralAppConfigData};
use crate::state::{Store, StoreData, StoryKey};
use serenity::futures::StreamExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

#[macro_use]
mod macros;
mod commands;
mod config;
mod language_parsing;
mod state;
mod stats;
mod utils;

#[group]
#[commands(init_channel, show_stats, show_channels, server_summary, feedback)]
struct General;

#[group]
#[commands(gen_wordcloud)]
struct WordCloud;

#[group]
#[commands(ping, ping_me, dump_messages)]
#[help_available(false)]
struct Debug;

struct Handler {
    tasks_running: AtomicBool,
}

#[async_trait]
impl EventHandler for Handler {
    async fn cache_ready(&self, ctx: Context, _guilds: Vec<GuildId>) {
        println!("Cache built successfully!");
        set_bot_activity(&ctx).await;
        if !self.tasks_running.load(Ordering::Relaxed) {
            store_replay(&ctx).await;
            let ctx = Arc::new(ctx);
            let ctx1 = Arc::clone(&ctx);
            let _ = tokio::spawn(async move {
                dictionary_update_worker(ctx1).await;
            });
            let ctx2 = Arc::clone(&ctx);
            let _ = tokio::spawn(async move {
                dump_state(ctx2).await;
            });
            self.tasks_running.swap(true, Ordering::Relaxed);
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }
}

async fn set_bot_activity(ctx: &Context) {
    let prefix = {
        let config_lock = {
            let data_read = ctx.data.read().await;
            data_read
                .get::<GeneralAppConfigData>()
                .expect("Expected StoryData in TypeMap.")
                .clone()
        };
        let config = config_lock.read().unwrap();
        config.prefix.clone()
    };
    ctx.set_activity(Activity::listening(&prefix)).await;
}

async fn store_replay(ctx: &Context) {
    let story_keys_with_last_message = {
        let store_lock = {
            let data_read = ctx.data.read().await;
            data_read
                .get::<StoreData>()
                .expect("Expected StoryData in TypeMap.")
                .clone()
        };
        let store = store_lock.read().unwrap();
        store.story_keys_with_last_message()
    };
    info!("initialising store!");
    let mut new_messages = HashMap::<StoryKey, Vec<Message>>::new();
    for (story_key, last_message_id) in story_keys_with_last_message.into_iter() {
        let (_, channel_id) = story_key;
        let channel_name = channel_id.name(&ctx.cache).await.unwrap();
        info!("Checking for missed messages in {}", channel_name);
        let msgs = channel_id
            .messages(&ctx.http, |get_messages_builder| {
                get_messages_builder.after(last_message_id).limit(50)
            })
            .await
            .unwrap();
        info!("Got {} messages", msgs.len());
        if msgs.len() > 0 {
            new_messages.insert(story_key, msgs);
        }
    }
    info!(
        "Retrieved {} messages across all channels to populate:",
        new_messages.len()
    );
    let store_lock = {
        let data_read = ctx.data.read().await;
        data_read
            .get::<StoreData>()
            .expect("Expected StoryData in TypeMap.")
            .clone()
    };
    let mut store = store_lock.write().unwrap();
    for (story_key, messages) in new_messages {
        //Since we got these messages from the store, we can expect the key to exist
        let channel_data = store.get_channel_data_mut(&story_key).unwrap();
        for message in messages {
            channel_data.update(&message);
        }
    }
    store.finish_replay();
    info!("Finished initialising");
}

async fn dictionary_update_worker(_ctx: Arc<Context>) {
    loop {
        println!("Dictionary update!");
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn dump_state(ctx: Arc<Context>) {
    loop {
        println!("Dumping state!");
        {
            let store_lock = {
                let data_read = ctx.data.read().await;
                data_read
                    .get::<StoreData>()
                    .expect("Expected StoryData in TypeMap.")
                    .clone()
            };
            let store = store_lock.read().unwrap();
            store.dump().unwrap();
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

fn maybe_start_python_wordcloud_worker(config: &GeneralAppConfig) {
    if let Some(word_cloud_config) = &config.wordcloud_config {
        let default_python_path = PathBuf::from(".");
        let python_path = word_cloud_config
            .venv_path
            .as_ref()
            .unwrap_or(&default_python_path);
        let python_wordcloud_worker =
            tokio::process::Command::new(word_cloud_config.python_path.as_os_str())
                .env("PYTHONPATH", python_path)
                .arg("wordcloud/word_cloud_worker.py")
                .arg(word_cloud_config.request_path.as_os_str())
                .arg(word_cloud_config.generated_image_path.as_os_str())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
        wordcloud_worker_chaperone(python_wordcloud_worker);
    }
}
fn wordcloud_worker_chaperone(mut child: tokio::process::Child) {
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
    let _ = tokio::spawn(async move {
        info!("printing wordcloud worker stdout to info");
        while let Some(line) = stdout.next_line().await.unwrap() {
            println!("Wordcloud Worker stdout >> {}", line);
        }
        info!("wordcloud worker stdout closed");
    });
    let _ = tokio::spawn(async move {
        info!("printing wordcloud worker stderr to info");
        while let Some(line) = stderr.next_line().await.unwrap() {
            println!("Wordcloud Worker stderr >> {}", line)
        }
        info!("wordcloud worker stderr closed");
    });
}

#[tokio::main]
async fn main() {
    let config = GeneralAppConfig::load(Path::new("config.ron")).unwrap();
    //Start python wordcloud worker
    maybe_start_python_wordcloud_worker(&config);
    let _ = SimpleLogger::init(LevelFilter::Info, simplelog::Config::default());
    let token = env::var("BOT_TOKEN").expect("Need bot token");
    let http = Http::new_with_token(&token);
    let app_info = http.get_current_application_info().await.unwrap();
    println!("{:#?}", app_info);
    let framework = StandardFramework::new()
        .configure(|c| c.prefix(&config.prefix).on_mention(Some(app_info.id)))
        .normal_message(on_regular_message)
        .unrecognised_command(on_unrecognised_command)
        .bucket("global-wordcloud-bucket", |b| b.limit(20).time_span(30))
        .await
        .help(&HELP)
        .group(&GENERAL_GROUP)
        .group(&DEBUG_GROUP)
        .group(&WORDCLOUD_GROUP);
    let mut client = Client::builder(&token)
        .event_handler(Handler {
            tasks_running: AtomicBool::new(false),
        })
        .framework(framework)
        .await
        .expect("Error creating client");

    // Insert the global data:
    {
        let mut data = client.data.write().await;
        let store = match Store::load() {
            Ok(store) => store,
            Err(e) => {
                panic!("Parse failed: {:#?}", e);
            }
        };
        data.insert::<StoreData>(Arc::new(RwLock::new(store)));
        data.insert::<GeneralAppConfigData>(Arc::new(RwLock::new(config)));
    }

    // start listening for events by starting a single shard
    if let Err(why) = client.start().await {
        println!("An error occurred while running the client: {:?}", why);
    }
}

#[hook]
async fn on_unrecognised_command(ctx: &Context, msg: &Message, unrecognised_command_name: &str) {
    msg.reply(
        ctx,
        format!(
            "Unknown command \"{}\". Try [help]",
            unrecognised_command_name
        ),
    )
    .await
    .unwrap();
}

#[help]
#[max_levenshtein_distance(3)]
async fn help(
    context: &Context,
    msg: &Message,
    args: Args,
    help_options: &'static HelpOptions,
    groups: &[&'static CommandGroup],
    owners: HashSet<UserId>,
) -> CommandResult {
    let _ = help_commands::with_embeds(context, msg, args, help_options, groups, owners).await;
    Ok(())
}

async fn update_stats_if_exist(story_key: StoryKey, ctx: &Context, message: &Message) {
    let store_lock = {
        let data_read = ctx.data.read().await;
        data_read
            .get::<StoreData>()
            .expect("Expected StoryData in TypeMap.")
            .clone()
    };
    let mut store = store_lock.write().unwrap();
    store.process_message(&story_key, message);
}

#[hook]
async fn on_regular_message(ctx: &Context, message: &Message) {
    //Update a stats if this channel is initialised
    if let Some(server_id) = message.guild_id {
        let story_key = (server_id, message.channel_id);
        update_stats_if_exist(story_key, ctx, message).await;
    }
}

#[command]
async fn ping(ctx: &Context, msg: &Message) -> CommandResult {
    msg.reply(ctx, "Pong!").await?;
    Ok(())
}

#[command("ping-me")]
#[checks("AdminOnly")]
async fn ping_me(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
    if let Ok(user) = args.single::<UserId>() {
        println!("Got: {}", user);
        msg.reply(ctx, format!("Got: {:?}", user)).await?;
    } else {
        msg.reply(ctx, "Failed to parsed username from the command's arg")
            .await?;
    }
    Ok(())
}

#[check]
#[name = "AdminOnly"]
async fn admin_only_check(
    _: &Context,
    msg: &Message,
    _: &mut Args,
    _: &CommandOptions,
) -> std::result::Result<(), Reason> {
    match ADMINS.contains(&msg.author.id.0) {
        true => Ok(()),
        false => Err(Reason::User("Only available to admins".to_string())),
    }
}
const ADMINS: [u64; 1] = [190534649548767243];
