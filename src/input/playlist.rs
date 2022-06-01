use regex::Regex;
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use serde_json::json;
use simplelog::*;

use crate::utils::{
    check_sync, gen_dummy, get_delta, get_sec, is_close, json_serializer::read_json,
    json_serializer::read_remote_json, modified_time, seek_and_length, validate_source,
    GlobalConfig, Media, Playlist, PlayoutStatus, DUMMY_LEN,
};

/// Struct for current playlist.
///
/// Here we prepare the init clip and build a iterator where we pull our clips.
#[derive(Debug)]
pub struct CurrentProgram {
    config: GlobalConfig,
    start_sec: f64,
    json_mod: Option<String>,
    json_path: Option<String>,
    json_date: String,
    pub nodes: Arc<Mutex<Vec<Media>>>,
    current_node: Media,
    index: Arc<AtomicUsize>,
    is_terminated: Arc<AtomicBool>,
    playout_stat: PlayoutStatus,
}

impl CurrentProgram {
    pub fn new(
        config: &GlobalConfig,
        playout_stat: PlayoutStatus,
        is_terminated: Arc<AtomicBool>,
        current_list: Arc<Mutex<Vec<Media>>>,
        global_index: Arc<AtomicUsize>,
    ) -> Self {
        let json: Playlist = if Regex::new(r"^https?://.*")
            .unwrap()
            .is_match(&config.playlist.path)
        {
            read_remote_json(config, None, is_terminated.clone(), true, 0.0)
        } else {
            read_json(config, None, is_terminated.clone(), true, 0.0)
        };

        *current_list.lock().unwrap() = json.program;
        *playout_stat.current_date.lock().unwrap() = json.date.clone();

        if *playout_stat.date.lock().unwrap() != json.date {
            let data = json!({
                "time_shift": 0.0,
                "date": json.date,
            });

            let json: String = serde_json::to_string(&data).expect("Serialize status data failed");
            fs::write(config.general.stat_file.clone(), &json).expect("Unable to write file");
        }

        Self {
            config: config.clone(),
            start_sec: json.start_sec.unwrap(),
            json_mod: json.modified,
            json_path: json.current_file,
            json_date: json.date,
            nodes: current_list,
            current_node: Media::new(0, String::new(), false),
            index: global_index,
            is_terminated,
            playout_stat,
        }
    }

    // Check if playlist file got updated, and when yes we reload it and setup everything in place.
    fn check_update(&mut self, seek: bool) {
        if self.json_path.is_none() {
            let json: Playlist = if Regex::new(r"^https?://.*")
                .unwrap()
                .is_match(&self.config.playlist.path)
            {
                read_remote_json(&self.config, None, self.is_terminated.clone(), seek, 0.0)
            } else {
                read_json(&self.config, None, self.is_terminated.clone(), seek, 0.0)
            };

            self.json_path = json.current_file;
            self.json_mod = json.modified;
            *self.nodes.lock().unwrap() = json.program;
        } else if Path::new(&self.json_path.clone().unwrap()).is_file() {
            let mod_time = modified_time(&self.json_path.clone().unwrap());

            if let Some(m) = mod_time {
                if !m.to_string().eq(&self.json_mod.clone().unwrap()) {
                    // when playlist has changed, reload it
                    info!(
                        "Reload playlist <b><magenta>{}</></b>",
                        self.json_path.clone().unwrap()
                    );

                    let json = read_json(
                        &self.config,
                        self.json_path.clone(),
                        self.is_terminated.clone(),
                        false,
                        0.0,
                    );

                    self.json_mod = json.modified;
                    *self.nodes.lock().unwrap() = json.program;

                    self.get_current_clip();
                    self.index.fetch_add(1, Ordering::SeqCst);
                }
            }
        } else if Regex::new(r"^https?://.*")
            .unwrap()
            .is_match(&self.json_path.clone().unwrap())
        {
            let client = reqwest::blocking::Client::new();

            let resp = client.head(self.json_path.clone().unwrap()).send().unwrap();
            if resp.status().is_success() {
                let headers = resp.headers().clone();
                let last_modified = headers.get(reqwest::header::LAST_MODIFIED).unwrap();

                if !last_modified.eq(&self.json_mod.clone().unwrap()) {
                    let json = read_remote_json(
                        &self.config,
                        self.json_path.clone(),
                        self.is_terminated.clone(),
                        false,
                        0.0,
                    );

                    self.json_mod = json.modified;
                    *self.nodes.lock().unwrap() = json.program;

                    self.get_current_clip();
                    self.index.fetch_add(1, Ordering::SeqCst);
                }
            }
        } else {
            error!(
                "Playlist <b><magenta>{}</></b> not exists!",
                self.json_path.clone().unwrap()
            );
            let mut media = Media::new(0, String::new(), false);
            media.begin = Some(get_sec());
            media.duration = DUMMY_LEN;
            media.out = DUMMY_LEN;

            self.json_path = None;
            *self.nodes.lock().unwrap() = vec![media.clone()];
            self.current_node = media;
            self.playout_stat.list_init.store(true, Ordering::SeqCst);
            self.index.store(0, Ordering::SeqCst);
        }
    }

    // Check if day is past and it is time for a new playlist.
    fn check_for_next_playlist(&mut self) {
        let current_time = get_sec();
        let start_sec = self.config.playlist.start_sec.unwrap();
        let target_length = self.config.playlist.length_sec.unwrap();
        let (delta, total_delta) = get_delta(&self.config, &current_time);
        let mut duration = self.current_node.out;

        if self.current_node.duration > self.current_node.out {
            duration = self.current_node.duration
        }

        let mut next_start = self.current_node.begin.unwrap() - start_sec + duration + delta;

        if self.index.load(Ordering::SeqCst) == self.nodes.lock().unwrap().len() - 1 {
            next_start += self.config.general.stop_threshold;
        }

        if next_start >= target_length
            || is_close(total_delta, 0.0, 2.0)
            || is_close(total_delta, target_length, 2.0)
        {
            let json: Playlist = if Regex::new(r"^https?://.*")
                .unwrap()
                .is_match(&self.config.playlist.path)
            {
                read_remote_json(
                    &self.config,
                    None,
                    self.is_terminated.clone(),
                    false,
                    next_start,
                )
            } else {
                read_json(
                    &self.config,
                    None,
                    self.is_terminated.clone(),
                    false,
                    next_start,
                )
            };

            let data = json!({
                "time_shift": 0.0,
                "date": json.date,
            });

            *self.playout_stat.current_date.lock().unwrap() = json.date.clone();
            *self.playout_stat.time_shift.lock().unwrap() = 0.0;
            let status_data: String =
                serde_json::to_string(&data).expect("Serialize status data failed");
            fs::write(self.config.general.stat_file.clone(), &status_data)
                .expect("Unable to write file");

            self.json_path = json.current_file.clone();
            self.json_mod = json.modified;
            self.json_date = json.date;
            *self.nodes.lock().unwrap() = json.program;
            self.index.store(0, Ordering::SeqCst);

            if json.current_file.is_none() {
                self.playout_stat.list_init.store(true, Ordering::SeqCst);
            }
        }
    }

    // Check if last and/or next clip is a advertisement.
    fn last_next_ad(&mut self) {
        let index = self.index.load(Ordering::SeqCst);
        let current_list = self.nodes.lock().unwrap();

        if index + 1 < current_list.len()
            && &current_list[index + 1].category.clone().unwrap_or_default() == "advertisement"
        {
            self.current_node.next_ad = Some(true);
        }

        if index > 0
            && index < current_list.len()
            && &current_list[index - 1].category.clone().unwrap_or_default() == "advertisement"
        {
            self.current_node.last_ad = Some(true);
        }
    }

    // Get current time and when we are before start time,
    // we add full seconds of a day to it.
    fn get_current_time(&mut self) -> f64 {
        let mut time_sec = get_sec();

        if time_sec < self.start_sec {
            time_sec += self.config.playlist.length_sec.unwrap()
        }

        time_sec
    }

    // On init or reload we need to seek for the current clip.
    fn get_current_clip(&mut self) {
        let mut time_sec = self.get_current_time();
        let shift = self.playout_stat.time_shift.lock().unwrap();

        if *self.playout_stat.current_date.lock().unwrap()
            == *self.playout_stat.date.lock().unwrap()
            && *shift != 0.0
        {
            info!("Shift playlist start for <yellow>{}</> seconds", *shift);
            time_sec += *shift;
        }

        for (i, item) in self.nodes.lock().unwrap().iter_mut().enumerate() {
            if item.begin.unwrap() + item.out - item.seek > time_sec {
                self.playout_stat.list_init.store(false, Ordering::SeqCst);
                self.index.store(i, Ordering::SeqCst);

                break;
            }
        }
    }

    // Prepare init clip.
    fn init_clip(&mut self) {
        self.get_current_clip();

        if !self.playout_stat.list_init.load(Ordering::SeqCst) {
            let time_sec = self.get_current_time();
            let index = self.index.fetch_add(1, Ordering::SeqCst);

            // de-instance node to preserve original values in list
            let mut node_clone = self.nodes.lock().unwrap()[index].clone();

            node_clone.seek = time_sec - node_clone.begin.unwrap();
            self.current_node = handle_list_init(&self.config, node_clone);
        }
    }
}

/// Build the playlist iterator
impl Iterator for CurrentProgram {
    type Item = Media;

    fn next(&mut self) -> Option<Self::Item> {
        if self.playout_stat.list_init.load(Ordering::SeqCst) {
            self.check_update(true);

            if self.json_path.is_some() {
                self.init_clip();
            }

            if self.playout_stat.list_init.load(Ordering::SeqCst) {
                // On init load, playlist could be not long enough,
                // so we check if we can take the next playlist already,
                // or we fill the gap with a dummy.
                let list_length = self.nodes.lock().unwrap().len();
                self.current_node = self.nodes.lock().unwrap()[list_length - 1].clone();
                self.check_for_next_playlist();

                let new_node = self.nodes.lock().unwrap()[list_length - 1].clone();
                let new_length = new_node.begin.unwrap() + new_node.duration;

                if new_length
                    >= self.config.playlist.length_sec.unwrap()
                        + self.config.playlist.start_sec.unwrap()
                {
                    self.init_clip();
                } else {
                    let mut current_time = get_sec();
                    let (_, total_delta) = get_delta(&self.config, &current_time);
                    let mut duration = DUMMY_LEN;

                    if DUMMY_LEN > total_delta {
                        duration = total_delta;
                        self.playout_stat.list_init.store(false, Ordering::SeqCst);
                    }

                    if self.config.playlist.start_sec.unwrap() > current_time {
                        current_time += self.config.playlist.length_sec.unwrap() + 1.0;
                    }

                    let mut media = Media::new(0, String::new(), false);
                    media.begin = Some(current_time);
                    media.duration = duration;
                    media.out = duration;

                    self.current_node = gen_source(&self.config, media);
                    self.nodes.lock().unwrap().push(self.current_node.clone());
                    self.index
                        .store(self.nodes.lock().unwrap().len(), Ordering::SeqCst);
                }
            }

            self.last_next_ad();

            return Some(self.current_node.clone());
        }

        if self.index.load(Ordering::SeqCst) < self.nodes.lock().unwrap().len() {
            self.check_for_next_playlist();
            let mut is_last = false;
            let index = self.index.load(Ordering::SeqCst);

            if index == self.nodes.lock().unwrap().len() - 1 {
                is_last = true
            }

            self.current_node = timed_source(
                self.nodes.lock().unwrap()[index].clone(),
                &self.config,
                is_last,
                &self.playout_stat,
            );
            self.last_next_ad();
            self.index.fetch_add(1, Ordering::SeqCst);

            // update playlist should happen after current clip,
            // to prevent unknown behaviors.
            self.check_update(false);
            Some(self.current_node.clone())
        } else {
            let last_playlist = self.json_path.clone();
            let last_ad = self.current_node.last_ad;
            self.check_for_next_playlist();
            let (_, total_delta) =
                get_delta(&self.config, &self.config.playlist.start_sec.unwrap());

            if last_playlist == self.json_path
                && total_delta.abs() > self.config.general.stop_threshold
            {
                // Test if playlist is to early finish,
                // and if we have to fill it with a placeholder.
                let index = self.index.load(Ordering::SeqCst);
                self.current_node = Media::new(index, String::new(), false);
                self.current_node.begin = Some(get_sec());
                let mut duration = total_delta.abs();

                if duration > DUMMY_LEN {
                    duration = DUMMY_LEN;
                }
                self.current_node.duration = duration;
                self.current_node.out = duration;
                self.current_node = gen_source(&self.config, self.current_node.clone());
                self.nodes.lock().unwrap().push(self.current_node.clone());
                self.last_next_ad();

                self.current_node.last_ad = last_ad;
                self.current_node.add_filter(&self.config);

                self.index.fetch_add(1, Ordering::SeqCst);

                return Some(self.current_node.clone());
            }

            self.index.store(0, Ordering::SeqCst);
            self.current_node = gen_source(&self.config, self.nodes.lock().unwrap()[0].clone());
            self.last_next_ad();
            self.current_node.last_ad = last_ad;

            self.index.store(1, Ordering::SeqCst);

            Some(self.current_node.clone())
        }
    }
}

/// Prepare input clip:
///
/// - check begin and length from clip
/// - return clip only if we are in 24 hours time range
fn timed_source(
    node: Media,
    config: &GlobalConfig,
    last: bool,
    playout_stat: &PlayoutStatus,
) -> Media {
    let (delta, total_delta) = get_delta(config, &node.begin.unwrap());
    let mut shifted_delta = delta;
    let mut new_node = node.clone();
    new_node.process = Some(false);

    if config.playlist.length.contains(':') {
        let time_shift = playout_stat.time_shift.lock().unwrap();

        if *playout_stat.current_date.lock().unwrap() == *playout_stat.date.lock().unwrap()
            && *time_shift != 0.0
        {
            shifted_delta = delta - *time_shift;

            debug!("Delta: <yellow>{shifted_delta:.3}</>, shifted: <yellow>{delta:.3}</>");
        } else {
            debug!("Delta: <yellow>{shifted_delta:.3}</>");
        }

        debug!("Total time remaining: <yellow>{total_delta:.3}</>");

        let sync = check_sync(config, shifted_delta);

        if !sync {
            new_node.cmd = None;

            return new_node;
        }
    }

    if (total_delta > node.out - node.seek && !last)
        || node.index.unwrap() < 2
        || !config.playlist.length.contains(':')
    {
        // when we are in the 24 hour range, get the clip
        new_node = gen_source(config, node);
        new_node.process = Some(true);
    } else if total_delta <= 0.0 {
        info!("Begin is over play time, skip: {}", node.source);
    } else if total_delta < node.duration - node.seek || last {
        new_node = handle_list_end(node, total_delta);
    }

    new_node
}

/// Generate the source CMD, or when clip not exist, get a dummy.
fn gen_source(config: &GlobalConfig, mut node: Media) -> Media {
    if validate_source(&node.source) {
        node.add_probe();
        node.cmd = Some(seek_and_length(
            node.source.clone(),
            node.seek,
            node.out,
            node.duration,
        ));
        node.add_filter(config);
    } else {
        if node.source.chars().count() == 0 {
            warn!(
                "Generate filler with <yellow>{:.2}</> seconds length!",
                node.out - node.seek
            );
        } else {
            error!("File not found: <b><magenta>{}</></b>", node.source);
        }
        let (source, cmd) = gen_dummy(config, node.out - node.seek);
        node.source = source;
        node.cmd = Some(cmd);
        node.add_filter(config);
    }

    node
}

/// Handle init clip, but this clip can be the last one in playlist,
/// this we have to figure out and calculate the right length.
fn handle_list_init(config: &GlobalConfig, mut node: Media) -> Media {
    debug!("Playlist init");
    let (_, total_delta) = get_delta(config, &node.begin.unwrap());
    let mut out = node.out;

    if node.out - node.seek > total_delta {
        out = total_delta + node.seek;
    }

    node.out = out;
    gen_source(config, node)
}

/// when we come to last clip in playlist,
/// or when we reached total playtime,
/// we end up here
fn handle_list_end(mut node: Media, total_delta: f64) -> Media {
    debug!("Playlist end");

    let mut out = if node.seek > 0.0 {
        node.seek + total_delta
    } else {
        total_delta
    };

    // prevent looping
    if out > node.duration {
        out = node.duration
    } else {
        warn!("Clip length is not in time, new duration is: <yellow>{total_delta:.2}</>")
    }

    if node.duration > total_delta && total_delta > 1.0 && node.duration - node.seek >= total_delta
    {
        node.out = out;
    } else if node.duration > total_delta && total_delta < 1.0 {
        warn!(
            "Last clip less then 1 second long, skip: <b><magenta>{}</></b>",
            node.source
        );
        node.out = out;
        node.cmd = Some(seek_and_length(
            node.source.clone(),
            node.seek,
            node.out,
            node.duration,
        ));

        node.process = Some(false);

        return node;
    } else {
        warn!("Playlist is not long enough: <yellow>{total_delta:.2}</> seconds needed");
    }

    node.process = Some(true);
    node.cmd = Some(seek_and_length(
        node.source.clone(),
        node.seek,
        node.out,
        node.duration,
    ));

    node
}
