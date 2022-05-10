use std::sync::atomic::Ordering;

use jsonrpc_http_server::{
    hyper,
    jsonrpc_core::{IoHandler, Params, Value},
    AccessControlAllowOrigin, DomainsValidation, Response, RestApi, ServerBuilder,
};
use serde_json::{json, Map};
use simplelog::*;

use crate::utils::{
    get_delta, get_sec, sec_to_time, write_status, GlobalConfig, Media, PlayerControl,
    PlayoutStatus, ProcessControl,
};

/// map media struct to json object
fn get_media_map(media: Media) -> Value {
    json!({
        "seek": media.seek,
        "out": media.out,
        "duration": media.duration,
        "category": media.category,
        "source": media.source,
    })
}

/// prepare json object for response
fn get_data_map(config: &GlobalConfig, media: Media) -> Map<String, Value> {
    let mut data_map = Map::new();
    let begin = media.begin.unwrap_or(0.0);

    data_map.insert("play_mode".to_string(), json!(config.processing.mode));
    data_map.insert("index".to_string(), json!(media.index));
    data_map.insert("start_sec".to_string(), json!(begin));

    if begin > 0.0 {
        let played_time = get_sec() - begin;
        let remaining_time = media.out - played_time;

        data_map.insert("start_time".to_string(), json!(sec_to_time(begin)));
        data_map.insert("played_sec".to_string(), json!(played_time));
        data_map.insert("remaining_sec".to_string(), json!(remaining_time));
    }

    data_map.insert("current_media".to_string(), get_media_map(media));

    data_map
}

/// JSON RPC Server
///
/// A simple rpc server for getting status information and controlling player:
///
/// - current clip information
/// - jump to next clip
/// - get last clip
/// - reset player state to original clip
pub fn json_rpc_server(
    play_control: PlayerControl,
    playout_stat: PlayoutStatus,
    proc_control: ProcessControl,
) {
    let config = GlobalConfig::global();
    let mut io = IoHandler::default();
    let proc = proc_control.clone();

    io.add_sync_method("player", move |params: Params| {
        if let Params::Map(map) = params {
            let mut time_shift = playout_stat.time_shift.lock().unwrap();
            let current_date = playout_stat.current_date.lock().unwrap().clone();
            let mut date = playout_stat.date.lock().unwrap();

            // get next clip
            if map.contains_key("control") && &map["control"] == "next" {
                let index = play_control.index.load(Ordering::SeqCst);

                if index < play_control.current_list.lock().unwrap().len() {
                    if let Some(proc) = proc.decoder_term.lock().unwrap().as_mut() {
                        if let Err(e) = proc.kill() {
                            error!("Decoder {e:?}")
                        };

                        if let Err(e) = proc.wait() {
                            error!("Decoder {e:?}")
                        };

                        info!("Move to next clip");

                        let mut data_map = Map::new();
                        let mut media = play_control.current_list.lock().unwrap()[index].clone();
                        media.add_probe();

                        let (delta, _) = get_delta(&media.begin.unwrap_or(0.0));
                        *time_shift = delta;
                        *date = current_date.clone();
                        write_status(&current_date, delta);

                        data_map.insert("operation".to_string(), json!("move_to_next"));
                        data_map.insert("shifted_seconds".to_string(), json!(delta));
                        data_map.insert("media".to_string(), get_media_map(media));

                        return Ok(Value::Object(data_map));
                    }

                    return Ok(Value::String("Move failed".to_string()));
                }

                return Ok(Value::String("Last clip can not be skipped".to_string()));
            }

            // get last clip
            if map.contains_key("control") && &map["control"] == "back" {
                let index = play_control.index.load(Ordering::SeqCst);

                if index > 1 && play_control.current_list.lock().unwrap().len() > 1 {
                    if let Some(proc) = proc.decoder_term.lock().unwrap().as_mut() {
                        if let Err(e) = proc.kill() {
                            error!("Decoder {e:?}")
                        };

                        if let Err(e) = proc.wait() {
                            error!("Decoder {e:?}")
                        };

                        info!("Move to last clip");
                        let mut data_map = Map::new();
                        let mut media =
                            play_control.current_list.lock().unwrap()[index - 2].clone();
                        play_control.index.fetch_sub(2, Ordering::SeqCst);
                        media.add_probe();

                        let (delta, _) = get_delta(&media.begin.unwrap_or(0.0));
                        *time_shift = delta;
                        *date = current_date.clone();
                        write_status(&current_date, delta);

                        data_map.insert("operation".to_string(), json!("move_to_last"));
                        data_map.insert("shifted_seconds".to_string(), json!(delta));
                        data_map.insert("media".to_string(), get_media_map(media));

                        return Ok(Value::Object(data_map));
                    }

                    return Ok(Value::String("Move failed".to_string()));
                }

                return Ok(Value::String("Clip index out of range".to_string()));
            }

            // reset player state
            if map.contains_key("control") && &map["control"] == "reset" {
                if let Some(proc) = proc.decoder_term.lock().unwrap().as_mut() {
                    if let Err(e) = proc.kill() {
                        error!("Decoder {e:?}")
                    };

                    if let Err(e) = proc.wait() {
                        error!("Decoder {e:?}")
                    };

                    info!("Reset playout to original state");
                    let mut data_map = Map::new();
                    *time_shift = 0.0;
                    *date = current_date.clone();
                    playout_stat.list_init.store(true, Ordering::SeqCst);

                    write_status(&current_date, 0.0);

                    data_map.insert("operation".to_string(), json!("reset_playout_state"));

                    return Ok(Value::Object(data_map));
                }

                return Ok(Value::String("Reset playout state failed".to_string()));
            }

            // get infos about current clip
            if map.contains_key("media") && &map["media"] == "current" {
                if let Some(media) = play_control.current_media.lock().unwrap().clone() {
                    let data_map = get_data_map(config, media);

                    return Ok(Value::Object(data_map));
                };
            }

            // get infos about next clip
            if map.contains_key("media") && &map["media"] == "next" {
                let index = play_control.index.load(Ordering::SeqCst);

                if index < play_control.current_list.lock().unwrap().len() {
                    let media = play_control.current_list.lock().unwrap()[index].clone();

                    let data_map = get_data_map(config, media);

                    return Ok(Value::Object(data_map));
                }

                return Ok(Value::String("There is no next clip".to_string()));
            }

            // get infos about last clip
            if map.contains_key("media") && &map["media"] == "last" {
                let index = play_control.index.load(Ordering::SeqCst);

                if index > 1 && index - 2 < play_control.current_list.lock().unwrap().len() {
                    let media = play_control.current_list.lock().unwrap()[index - 2].clone();

                    let data_map = get_data_map(config, media);

                    return Ok(Value::Object(data_map));
                }

                return Ok(Value::String("There is no last clip".to_string()));
            }
        }

        Ok(Value::String("No, or wrong parameters set!".to_string()))
    });

    // build rpc server
    let server = ServerBuilder::new(io)
        .cors(DomainsValidation::AllowOnly(vec![
            AccessControlAllowOrigin::Null,
        ]))
        // add middleware, for authentication
        .request_middleware(|request: hyper::Request<hyper::Body>| {
            if request.headers().contains_key("authorization")
                && request.headers()["authorization"] == config.rpc_server.authorization
            {
                if request.uri() == "/status" {
                    println!("{:?}", request.headers().contains_key("authorization"));
                    Response::ok("Server running OK.").into()
                } else {
                    request.into()
                }
            } else {
                Response::bad_request("No authorization header or valid key found!").into()
            }
        })
        .rest_api(RestApi::Secure)
        .start_http(&config.rpc_server.address.parse().unwrap())
        .expect("Unable to start RPC server");

    *proc_control.rpc_handle.lock().unwrap() = Some(server.close_handle());

    server.wait();
}