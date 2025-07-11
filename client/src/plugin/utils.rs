use anyhow::Context;
use iced::futures::StreamExt;
use nucleo_matcher::{
    pattern::{Atom, AtomKind, CaseMatching, Normalization, Pattern},
    Matcher, Utf32Str,
};
use std::cmp::Reverse;

pub fn spawn<PluginType: Plugin + std::marker::Send + 'static>(
) -> iced::Subscription<crate::Message> {
    iced::Subscription::run(|| {
        iced::stream::channel(100, |plugin_channel_out| async move {
            let mut plugin = PluginType::new();

            let main_loop_result = plugin.main(plugin_channel_out).await;
            if let Err(error) = main_loop_result {
                log::error!(
                    target: PluginType::id(),
                    "{:?}", error,
                );
                panic!();
            }

            #[allow(clippy::never_loop)]
            loop {
                unreachable!();
            }
        })
    })
}

/// Fuzzy matches against the title of the entry, falling back to substring matching
/// against the meta of the entry if no match is found in the title.
fn fuzzy_match(query: &str, entries: Vec<crate::model::Entry>) -> Vec<crate::model::Entry> {
    let mut matcher_config = nucleo_matcher::Config::DEFAULT;
    matcher_config.prefer_prefix = true; // Higher score to matches earlier in the string
    let mut fuzzy_matcher = Matcher::new(matcher_config);
    let fuzzy_atom = Atom::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );

    let mut substring_matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
    let substring_atom = Atom::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Substring,
        true,
    );

    let mut buf = Vec::new();
    let mut filtered_entries = entries
        .into_iter()
        .flat_map(|entry| {
            // Attempt to fuzzy match against the title
            fuzzy_atom
                .score(
                    Utf32Str::new(entry.title.as_ref(), &mut buf),
                    &mut fuzzy_matcher,
                )
                .map(|score| score + 1000) // Always prefer title matches
                // Fallback to substring match against the meta
                .or_else(|| {
                    substring_atom.score(
                        Utf32Str::new(entry.meta.as_ref(), &mut buf),
                        &mut substring_matcher,
                    )
                })
                .map(|score| (score, entry))
        })
        .collect::<Vec<_>>();

    // Sort by score
    filtered_entries.sort_by_key(|(score, _entry)| Reverse(*score));

    filtered_entries
        .into_iter()
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>()
}

#[async_trait::async_trait]
pub trait Plugin {
    fn id() -> &'static str;
    fn priority() -> u32;
    fn title() -> &'static str;
    fn update_timeout() -> Option<std::time::Duration> {
        None
    }

    fn new() -> Self;

    fn entries(&self) -> Vec<crate::model::Entry>;

    fn set_entries(&mut self, entries: Vec<crate::model::Entry>);

    fn update_entries(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn plugin(
        &self,
        app_channel_out: &mut iced::futures::channel::mpsc::Sender<crate::model::PluginRequest>,
    ) -> crate::model::Plugin {
        crate::model::Plugin {
            id: String::from(Self::id()),
            priority: Self::priority(),
            title: String::from(Self::title()),
            app_channel_out: app_channel_out.clone(),
            entries: self.entries(),
        }
    }

    async fn main(
        &mut self,
        mut plugin_channel_out: iced::futures::channel::mpsc::Sender<crate::Message>,
    ) -> anyhow::Result<()> {
        self.update_entries()?;

        let (mut app_channel_out, mut plugin_channel_in) =
            iced::futures::channel::mpsc::channel(100);
        self.register_plugin(&mut plugin_channel_out, &mut app_channel_out)?;
        let mut last_query = String::from("");

        loop {
            self.update(
                &mut plugin_channel_out,
                &mut plugin_channel_in,
                &mut last_query,
            )
            .await?;
        }
    }

    fn register_plugin(
        &mut self,
        plugin_channel_out: &mut iced::futures::channel::mpsc::Sender<crate::Message>,
        app_channel_out: &mut iced::futures::channel::mpsc::Sender<crate::model::PluginRequest>,
    ) -> anyhow::Result<()> {
        plugin_channel_out
            .try_send(crate::Message::RegisterPlugin(self.plugin(app_channel_out)))
            .context("Failed to send message to register plugin.")?;

        Ok(())
    }

    async fn update(
        &mut self,
        plugin_channel_out: &mut iced::futures::channel::mpsc::Sender<crate::Message>,
        plugin_channel_in: &mut iced::futures::channel::mpsc::Receiver<crate::model::PluginRequest>,
        last_query: &mut String,
    ) -> anyhow::Result<()> {
        let plugin_request_future = plugin_channel_in.select_next_some();
        let plugin_request = match Self::update_timeout() {
            Some(update_timeout) => {
                async_std::future::timeout(update_timeout, plugin_request_future)
                    .await
                    .unwrap_or(crate::model::PluginRequest::Timeout)
            }
            None => plugin_request_future.await,
        };

        match plugin_request {
            crate::model::PluginRequest::Search(query) => {
                self.search(&query, plugin_channel_out)?;
                *last_query = query;
            }
            crate::model::PluginRequest::Timeout => {
                self.update_entries()?;
                self.search(last_query, plugin_channel_out)?;
            }
            crate::model::PluginRequest::Activate(entry) => {
                self.activate(entry, plugin_channel_out)?
            }
        }

        return Ok(());
    }

    fn sort(&mut self) {
        let mut entries = self.entries();
        entries.sort_by_key(|entry| entry.title.clone().to_lowercase());
        self.set_entries(entries)
    }

    fn search(
        &mut self,
        query: &str,
        plugin_channel_out: &mut iced::futures::channel::mpsc::Sender<crate::Message>,
    ) -> anyhow::Result<()> {
        let filtered_entries = fuzzy_match(query, self.entries());

        plugin_channel_out
            .try_send(crate::Message::UpdateEntries(
                String::from(Self::id()),
                filtered_entries,
            ))
            .context(format!(
                "Failed to send message to update entries while searching for '{}'.",
                query
            ))?;

        Ok(())
    }

    fn activate(
        &mut self,
        _entry: crate::model::Entry,
        _plugin_channel_out: &mut iced::futures::channel::mpsc::Sender<crate::Message>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

pub fn read_index_file<T>(file_name: &str) -> anyhow::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let cache_directory = settings::centerpiece_cache_directory()?;
    let index_file_path = format!("{cache_directory}/{file_name}");

    let index_file =
        std::fs::File::open(index_file_path).context("Error while opening index file")?;

    let reader = std::io::BufReader::new(index_file);
    let git_repository_paths_result: Result<T, _> = serde_json::from_reader(reader);
    if let Err(error) = git_repository_paths_result {
        log::error!(
            error = log::error!("{:?}", error);
            "Error while reading index file",
        );
        panic!();
    }
    Ok(git_repository_paths_result.unwrap())
}
