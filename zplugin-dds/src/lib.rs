//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//
use async_std::channel::{unbounded, Receiver, Sender};
use async_std::task;
use clap::{Arg, ArgMatches};
use cyclors::*;
use futures::prelude::*;
use futures::select;
use git_version::git_version;
use log::{debug, info, warn};
use regex::Regex;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use zenoh::net::runtime::Runtime;
use zenoh::net::utils::resource_name;
use zenoh::net::Reliability as ZReliability;
use zenoh::net::*;
use zenoh::{GetRequest, Path, PathExpr, Value, Zenoh};
use zenoh_ext::net::group::{Group, GroupEvent, JoinEvent, Member};
use zenoh_ext::net::{
    PublicationCache, QueryingSubscriber, SessionExt, PUBLICATION_CACHE_QUERYABLE_KIND,
};
use zenoh_plugin_trait::{prelude::*, PluginId};

mod dds_mgt;
mod qos;
use dds_mgt::*;
use qos::*;

pub const GIT_VERSION: &str = git_version!(prefix = "v", cargo_prefix = "v");

lazy_static::lazy_static!(
    pub static ref LONG_VERSION: String = format!("{} built with {}", GIT_VERSION, env!("RUSTC_VERSION"));
);

const GROUP_NAME: &str = "zenoh-plugin-dds";
const GROUP_DEFAULT_LEASE: &str = "3";
lazy_static::lazy_static!(
    static ref DDS_DOMAIN_DEFAULT_STR: String = DDS_DOMAIN_DEFAULT.to_string();
);
const PUB_CACHE_QUERY_PREFIX: &str = "/zenoh_dds_plugin/pub_cache";

pub struct DDSPlugin;

impl Plugin for DDSPlugin {
    type Requirements = Vec<Arg<'static, 'static>>;

    type StartArgs = (Runtime, ArgMatches<'static>);

    fn compatibility() -> zenoh_plugin_trait::PluginId {
        PluginId {
            uid: "zenoh-dds-plugin",
        }
    }

    fn get_requirements() -> Self::Requirements {
        get_expected_args()
    }

    fn start(
        (runtime, args): &Self::StartArgs,
    ) -> Result<Box<dyn std::any::Any + Send + Sync>, Box<dyn std::error::Error>> {
        async_std::task::spawn(run(runtime.clone(), args.to_owned()));
        Ok(Box::new(()))
    }
}

zenoh_plugin_trait::declare_plugin!(DDSPlugin);

// NOTE: temporary hack for static link of DDS plugin in zenoh-bridge-dds, thus it can call this function
// instead of relying on #[no_mangle] functions that will conflicts with those defined in REST plugin.
// TODO: remove once eclipse-zenoh/zenoh#89 is implemented
pub fn get_expected_args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    vec![
        Arg::from_usage(
            "--dds-scope=[String]   'A string used as prefix to scope DDS traffic.'"
        ).default_value(""),
        Arg::from_usage(
            "--dds-generalise-pub=[String]...   'A list of key expression to use for generalising publications (usable multiple times).'"
        ),
        Arg::from_usage(
            "--dds-generalise-sub=[String]...   'A list of key expression to use for generalising subscriptions (usable multiple times).'"
        ),
        Arg::from_usage(
            "--dds-domain=[ID]   'The DDS Domain ID (if using with ROS this should be the same as ROS_DOMAIN_ID).'"
        ).default_value(&*DDS_DOMAIN_DEFAULT_STR),
        Arg::from_usage(
            "--dds-allow=[String]   'A regular expression matching the set of 'partition/topic-name' that should be bridged. \
            By default, all partitions and topic are allowed. \
            Examples of expressions: '.*/TopicA', 'Partition-?/.*', 'cmd_vel|rosout'...'"
        ),
        Arg::from_usage(
            "--dds-group-member-id=[ID]   'A custom identifier for the bridge, that will be used in group management (if not specified, the zenoh UUID is used).'"
        ),
        Arg::from_usage(
            "--dds-group-lease=[Duration]   'The lease duration (in seconds) used in group management for all DDS plugins.'"
        ).default_value(GROUP_DEFAULT_LEASE),
    ]
}

pub async fn run(runtime: Runtime, args: ArgMatches<'_>) {
    // Try to initiate login.
    // Required in case of dynamic lib, otherwise no logs.
    // But cannot be done twice in case of static link.
    let _ = env_logger::try_init();
    debug!("DDS plugin {}", LONG_VERSION.as_str());

    let scope = args.value_of("dds-scope").unwrap().to_string();

    let domain_id_str = args.value_of("dds-domain").unwrap();
    let domain_id = match domain_id_str.parse::<u32>() {
        Ok(adid) => adid,
        Err(_) => panic!("ERROR: {} is not a valid domain ID ", domain_id_str),
    };

    let group_lease_str = args.value_of("dds-group-lease").unwrap();
    let group_lease = match group_lease_str.parse::<u64>() {
        Ok(lease) => Duration::from_secs(lease),
        Err(_) => panic!(
            "ERROR: {} is not a valid lease duration in seconds ",
            group_lease_str
        ),
    };

    let allow_re = if let Some(res) = args.value_of("dds-allow") {
        match Regex::new(res) {
            Ok(re) => Some(re),
            Err(e) => {
                panic!("Unable to compile allow regular expression, please see error details below:\n {:?}\n", e)
            }
        }
    } else {
        None
    };

    let join_subscriptions: Vec<String> = args
        .values_of("dds-generalise-sub")
        .unwrap_or_default()
        .map(|s| s.to_string())
        .collect::<Vec<String>>();
    let join_publications: Vec<String> = args
        .values_of("dds-generalise-pub")
        .unwrap_or_default()
        .map(|s| s.to_string())
        .collect::<Vec<String>>();

    // open zenoh-net Session (with local routing disabled to avoid loops)
    let zsession =
        Arc::new(Session::init(runtime, false, join_subscriptions, join_publications).await);

    // create group member
    let zid = zsession.id().await;
    let member_id = args.value_of("dds-group-member-id").unwrap_or(&zid);
    let member = Member::new(member_id).lease(group_lease);

    // create DDS Participant
    let dp = unsafe { dds_create_participant(domain_id, std::ptr::null(), std::ptr::null()) };

    let dds_plugin = DdsPlugin {
        scope,
        domain_id,
        allow_re,
        zsession: &zsession,
        member,
        dp,
        dds_writer: HashMap::<String, DdsEntity>::new(),
        dds_reader: HashMap::<String, DdsEntity>::new(),
        routes_from_dds: HashMap::<String, FromDDSRoute>::new(),
        routes_to_dds: HashMap::<String, ToDDSRoute>::new(),
        admin_space: HashMap::<String, AdminRef>::new(),
    };

    dds_plugin.run().await;
}

// An reference used in admin space to point to a struct (DdsEntity or Route) stored in another map
enum AdminRef {
    DdsWriterEntity(String),
    DdsReaderEntity(String),
    FromDdsRoute(String),
    ToDdsRoute(String),
    Config,
    Version,
}

enum ZPublisher<'a> {
    Publisher(Publisher<'a>),
    PublicationCache(PublicationCache<'a>),
}

// a route from DDS
#[derive(Serialize)]
struct FromDDSRoute<'a> {
    // the local DDS Reader created to match the discovered user's DDS Writers
    #[serde(skip)]
    dds_reader: dds_entity_t,
    // the zenoh publisher used to re-publish to zenoh the data received by the DDS Reader
    #[serde(skip)]
    _zenoh_publisher: ZPublisher<'a>,
    // the DDS entity (local Writer or remote Reader) that led to create this route when discovered (admin path)
    initiated_by: String,
    // the list of discovered user's DDS writers (admin paths) that are routed by this route
    routed_writers: Vec<String>,
}

impl Drop for FromDDSRoute<'_> {
    fn drop(&mut self) {
        if let Err(e) = delete_dds_entity(self.dds_reader) {
            warn!("Error dropping a route from DDS: {}", e);
        }
    }
}

enum ZSubscriber<'a> {
    Subscriber(Subscriber<'a>),
    QueryingSubscriber(QueryingSubscriber<'a>),
}

// a route to DDS
#[derive(Serialize)]
struct ToDDSRoute<'a> {
    // the local DDS Writer created to match the discovered user's DDS Readers
    #[serde(skip)]
    dds_writer: dds_entity_t,
    // the zenoh subscriber receiving data to be re-published by the DDS Writer
    #[serde(skip)]
    zenoh_subscriber: ZSubscriber<'a>,
    // the DDS entity (local Reader or remote Writer) that led to create this route when discovered (admin path)
    initiated_by: String,
    // the list of discovered user's DDS readers (admin path) that are routed by this route
    routed_readers: Vec<String>,
}

impl Drop for ToDDSRoute<'_> {
    fn drop(&mut self) {
        if let Err(e) = delete_dds_entity(self.dds_writer) {
            warn!("Error dropping a route from DDS: {}", e);
        }
    }
}

struct DdsPlugin<'a> {
    scope: String,
    domain_id: u32,
    allow_re: Option<Regex>,
    // Note: &'a Arc<Session> here to keep the ownership of Session outside this struct
    // and be able to store the publishers/subscribers it creates in this same struct.
    zsession: &'a Arc<Session>,
    member: Member,
    dp: dds_entity_t,
    // maps of all discovered DDS entities (indexed by DDS key)
    dds_writer: HashMap<String, DdsEntity>,
    dds_reader: HashMap<String, DdsEntity>,
    // maps of established routes from/to DDS (indexed by zenoh resource key)
    routes_from_dds: HashMap<String, FromDDSRoute<'a>>,
    routes_to_dds: HashMap<String, ToDDSRoute<'a>>,
    // admin space: index is the admin_path (relative to admin_prefix)
    // value is the JSon string to return to queries.
    admin_space: HashMap<String, AdminRef>,
}

impl Serialize for DdsPlugin<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // return the plugin's config as a JSON struct
        let mut s = serializer.serialize_struct("dds", 3)?;
        s.serialize_field("domain_id", &self.domain_id)?;
        s.serialize_field("scope", &self.scope)?;
        s.serialize_field(
            "allow",
            &self
                .allow_re
                .as_ref()
                .map_or_else(|| "**".to_string(), |re| re.to_string()),
        )?;
        s.end()
    }
}

lazy_static::lazy_static! {
    static ref JSON_NULL_STR: String = serde_json::to_string(&serde_json::json!(null)).unwrap();
}

impl<'a> DdsPlugin<'a> {
    fn is_allowed(&self, zkey: &str) -> bool {
        match &self.allow_re {
            Some(re) => re.is_match(zkey),
            _ => true,
        }
    }

    fn get_admin_path(e: &DdsEntity, is_writer: bool) -> String {
        if is_writer {
            format!(
                "participant/{}/writer/{}/{}",
                e.participant_key, e.key, e.topic_name
            )
        } else {
            format!(
                "participant/{}/reader/{}/{}",
                e.participant_key, e.key, e.topic_name
            )
        }
    }

    fn insert_dds_writer(&mut self, admin_path: String, e: DdsEntity) {
        // insert reference in admin_space
        self.admin_space
            .insert(admin_path, AdminRef::DdsWriterEntity(e.key.clone()));

        // insert DdsEntity in dds_writer map
        self.dds_writer.insert(e.key.clone(), e);
    }

    fn remove_dds_writer(&mut self, key: &str) {
        // remove from dds_writer map
        if let Some(e) = self.dds_writer.remove(key) {
            // remove from admin_space
            let admin_path = DdsPlugin::get_admin_path(&e, true);
            self.admin_space.remove(&admin_path);

            // Remove this writer from all the active routes it was using (1 per partition)
            for route_status in e.routes.values() {
                if let RouteStatus::Routed(zkey) = route_status {
                    if let Some(route) = self.routes_from_dds.get_mut(zkey) {
                        route.routed_writers.retain(|k| k != &e.key);
                        // if route is no longer routing any writer, remove it
                        if route.routed_writers.is_empty() {
                            info!(
                                "Remove unused route: DDS '{}' => zenoh '{}'",
                                e.topic_name, zkey
                            );
                            self.routes_from_dds.remove(zkey);
                        }
                    }
                }
            }
        }
    }

    fn insert_dds_reader(&mut self, admin_path: String, e: DdsEntity) {
        // insert reference in admin_space
        self.admin_space
            .insert(admin_path, AdminRef::DdsReaderEntity(e.key.clone()));

        // insert DdsEntity in dds_reader map
        self.dds_reader.insert(e.key.clone(), e);
    }

    fn remove_dds_reader(&mut self, key: &str) {
        // remove from dds_reader map
        if let Some(e) = self.dds_reader.remove(key) {
            // remove from admin space
            let admin_path = DdsPlugin::get_admin_path(&e, false);
            self.admin_space.remove(&admin_path);

            // Remove this reader from all the active routes it was using (1 per partition)
            for route_status in e.routes.values() {
                if let RouteStatus::Routed(zkey) = route_status {
                    if let Some(route) = self.routes_to_dds.get_mut(zkey) {
                        route.routed_readers.retain(|k| k != &e.key);
                        // if route is no longer routing any reader, remove it
                        if route.routed_readers.is_empty() {
                            info!(
                                "Remove unused route: zenoh '{}' => DDS '{}'",
                                zkey, e.topic_name
                            );
                            self.routes_to_dds.remove(zkey);
                        }
                    }
                }
            }
        }
    }

    fn insert_route_from_dds(&mut self, zkey: &str, r: FromDDSRoute<'a>) {
        // insert reference in admin_space
        let path = format!("route/from_dds/{}", zkey);
        self.admin_space
            .insert(path, AdminRef::FromDdsRoute(zkey.to_string()));

        // insert route in routes_from_dds map
        self.routes_from_dds.insert(zkey.to_string(), r);
    }

    fn insert_route_to_dds(&mut self, zkey: &str, r: ToDDSRoute<'a>) {
        // insert reference in admin_space
        let path = format!("route/to_dds/{}", zkey);
        self.admin_space
            .insert(path, AdminRef::ToDdsRoute(zkey.to_string()));

        // insert route in routes_from_dds map
        self.routes_to_dds.insert(zkey.to_string(), r);
    }

    async fn try_add_route_from_dds(
        &mut self,
        zkey: &str,
        initiator_admin_path: &str,
        topic_name: &str,
        topic_type: &str,
        keyless: bool,
        reader_qos: Qos,
    ) -> RouteStatus {
        if !self.is_allowed(&zkey) {
            info!(
                "Ignoring Publication for resource {} as it is not allowed (see --allow option)",
                zkey
            );
            return RouteStatus::NotAllowed;
        }

        if self.routes_from_dds.contains_key(zkey) {
            // TODO: check if there is no QoS conflict with existing route
            debug!(
                "Route from DDS to resource {} already exists -- ignoring",
                zkey
            );
            return RouteStatus::Routed(zkey.to_string());
        }

        // declare the zenoh resource and the publisher
        let rkey = ResKey::RName(zkey.to_string());
        let nrid = self.zsession.declare_resource(&rkey).await.unwrap();
        let rid = ResKey::RId(nrid);
        let zenoh_publisher: ZPublisher<'a> = if reader_qos.durability.kind
            == DurabilityKind::TRANSIENT_LOCAL
        {
            #[allow(non_upper_case_globals)]
            let history = match (reader_qos.history.kind, reader_qos.history.depth) {
                (HistoryKind::KEEP_LAST, n) => n as usize,
                (HistoryKind::KEEP_ALL, _) => usize::MAX,
            };
            debug!(
                "Caching publications for TRANSIENT_LOCAL Writer on resource {} with history {}",
                zkey, history
            );
            ZPublisher::PublicationCache(
                self.zsession
                    .declare_publication_cache(&rid)
                    .history(history)
                    .queryable_prefix(format!("{}/{}", PUB_CACHE_QUERY_PREFIX, self.member.id()))
                    .await
                    .unwrap(),
            )
        } else {
            ZPublisher::Publisher(self.zsession.declare_publisher(&rid).await.unwrap())
        };

        info!(
            "New route: DDS '{}' => zenoh '{}' (rid={}) with type '{}'",
            topic_name, zkey, rid, topic_type
        );

        // create matching DDS Writer that forwards data coming from zenoh
        let dr: dds_entity_t = create_forwarding_dds_reader(
            self.dp,
            topic_name.to_string(),
            topic_type.to_string(),
            keyless,
            reader_qos,
            rid,
            self.zsession.clone(),
        );

        self.insert_route_from_dds(
            &zkey,
            FromDDSRoute {
                dds_reader: dr,
                _zenoh_publisher: zenoh_publisher,
                initiated_by: initiator_admin_path.to_string(),
                routed_writers: vec![],
            },
        );
        RouteStatus::Routed(zkey.to_string())
    }

    async fn try_add_route_to_dds(
        &mut self,
        zkey: &str,
        initiator_admin_path: &str,
        topic_name: &str,
        topic_type: &str,
        keyless: bool,
        writer_qos: Qos,
    ) -> RouteStatus {
        if let Some(re) = &self.allow_re {
            if !re.is_match(&zkey) {
                info!(
                        "Ignoring Subscription for resource {} as it is not allowed (see --allow option)",
                        zkey
                    );
                return RouteStatus::NotAllowed;
            }
        }

        if self.routes_to_dds.contains_key(zkey) {
            // TODO: check if there is no type or QoS conflict with existing route
            debug!(
                "Route from resource {} to DDS already exists -- ignoring",
                zkey
            );
            return RouteStatus::Routed(zkey.to_string());
        }

        info!(
            "New route: zenoh '{}' => DDS '{}' with type '{}'",
            zkey, topic_name, topic_type
        );

        // create matching DDS Writer that forwards data coming from zenoh
        let is_transient_local = writer_qos.durability.kind == DurabilityKind::TRANSIENT_LOCAL;
        let dw = create_forwarding_dds_writer(
            self.dp,
            topic_name.to_string(),
            topic_type.to_string(),
            keyless,
            writer_qos,
        );

        // create zenoh subscriber
        let rkey = ResKey::RName(zkey.to_string());
        let sub_info = SubInfo {
            reliability: ZReliability::Reliable,
            mode: SubMode::Push,
            period: None,
        };
        let (zenoh_subscriber, mut receiver): (_, Pin<Box<dyn Stream<Item = Sample> + Send>>) =
            if is_transient_local {
                debug!(
                    "Querying historical data for TRANSIENT_LOCAL Reader on resource {}",
                    zkey
                );
                let mut sub = self
                    .zsession
                    .declare_querying_subscriber(&rkey)
                    .query_reskey(format!("{}/*{}", PUB_CACHE_QUERY_PREFIX, zkey).into())
                    .wait()
                    .unwrap();
                let receiver = sub.receiver().clone();
                (ZSubscriber::QueryingSubscriber(sub), Box::pin(receiver))
            } else {
                let mut sub = self
                    .zsession
                    .declare_subscriber(&rkey, &sub_info)
                    .wait()
                    .unwrap();
                let receiver = sub.receiver().clone();
                (ZSubscriber::Subscriber(sub), Box::pin(receiver))
            };

        let ton = topic_name.to_string();
        let tyn = topic_type.to_string();
        let keyless = keyless;
        let dp = self.dp;
        task::spawn(async move {
            while let Some(d) = receiver.next().await {
                log::trace!("Route data to DDS '{}'", &ton);
                unsafe {
                    let bs = d.payload.to_vec();
                    // As per the Vec documentation (see https://doc.rust-lang.org/std/vec/struct.Vec.html#method.into_raw_parts)
                    // the only way to correctly releasing it is to create a vec using from_raw_parts
                    // and then have its destructor do the cleanup.
                    // Thus, while tempting to just pass the raw pointer to cyclone and then free it from C,
                    // that is not necessarily safe or guaranteed to be leak free.
                    // TODO replace when stable https://github.com/rust-lang/rust/issues/65816
                    let (ptr, len, capacity) = vec_into_raw_parts(bs);
                    let cton = CString::new(ton.clone()).unwrap().into_raw();
                    let ctyn = CString::new(tyn.clone()).unwrap().into_raw();
                    let st = cdds_create_blob_sertopic(
                        dp,
                        cton as *mut std::os::raw::c_char,
                        ctyn as *mut std::os::raw::c_char,
                        keyless,
                    );
                    drop(CString::from_raw(cton));
                    drop(CString::from_raw(ctyn));
                    let fwdp =
                        cdds_ddsi_payload_create(st, ddsi_serdata_kind_SDK_DATA, ptr, len as u64);
                    dds_writecdr(dw, fwdp as *mut ddsi_serdata);
                    drop(Vec::from_raw_parts(ptr, len, capacity));
                    cdds_sertopic_unref(st);
                };
            }
        });

        self.insert_route_to_dds(
            &zkey,
            ToDDSRoute {
                dds_writer: dw,
                zenoh_subscriber,
                initiated_by: initiator_admin_path.to_string(),
                routed_readers: vec![],
            },
        );
        RouteStatus::Routed(zkey.to_string())
    }

    fn get_admin_value(&self, admin_ref: &AdminRef) -> Option<Value> {
        match admin_ref {
            AdminRef::DdsReaderEntity(key) => self
                .dds_reader
                .get(key)
                .map(|e| Value::Json(serde_json::to_string(e).unwrap())),
            AdminRef::DdsWriterEntity(key) => self
                .dds_writer
                .get(key)
                .map(|e| Value::Json(serde_json::to_string(e).unwrap())),
            AdminRef::FromDdsRoute(zkey) => self
                .routes_from_dds
                .get(zkey)
                .map(|e| Value::Json(serde_json::to_string(e).unwrap())),
            AdminRef::ToDdsRoute(zkey) => self
                .routes_to_dds
                .get(zkey)
                .map(|e| Value::Json(serde_json::to_string(e).unwrap())),
            AdminRef::Config => Some(Value::Json(serde_json::to_string(self).unwrap())),
            AdminRef::Version => Some(Value::Json(format!(r#""{}""#, LONG_VERSION.as_str()))),
        }
    }

    async fn treat_admin_query(&self, get_request: GetRequest, admin_path_prefix: &str) {
        debug!("Query on admin space: {:?}", get_request.selector);

        // get the list of sub-path expressions that will match the same stored keys than
        // the selector, if those keys had the path_prefix.
        let path_exprs =
            Self::get_sub_path_exprs(get_request.selector.path_expr.as_str(), admin_path_prefix);

        // Get all matching keys/values
        let mut kvs: Vec<(&str, Value)> = Vec::with_capacity(path_exprs.len());
        for path_expr in path_exprs {
            if path_expr.contains('*') {
                // iterate over all admin space to find matching keys
                for (path, admin_ref) in self.admin_space.iter() {
                    if resource_name::intersect(path_expr, path) {
                        if let Some(v) = self.get_admin_value(admin_ref) {
                            kvs.push((path, v));
                        }
                    }
                }
            } else {
                // path_expr correspond to 1 key - just get it.
                if let Some(admin_ref) = self.admin_space.get(path_expr) {
                    if let Some(v) = self.get_admin_value(admin_ref) {
                        kvs.push((path_expr, v));
                    }
                }
            }
        }

        // send replies
        for (path, v) in kvs.drain(..) {
            let admin_path = Path::try_from(format!("{}{}", admin_path_prefix, path)).unwrap();
            // support the case of empty fragment in Selector (e.g.: "/@/**?[]"), returning 'null' value in such case
            let value = match &get_request.selector.fragment {
                Some(f) if f.is_empty() => Value::Json((*JSON_NULL_STR).clone()),
                _ => v,
            };
            get_request.reply(admin_path, value);
        }
    }

    pub fn get_sub_path_exprs<'s>(path_expr: &'s str, prefix: &str) -> Vec<&'s str> {
        if let Some(remaining) = path_expr.strip_prefix(prefix) {
            vec![remaining]
        } else {
            let mut result = vec![];
            for (i, c) in path_expr.char_indices().rev() {
                if c == '/' || i == path_expr.len() - 1 {
                    let sub_part = &path_expr[..i + 1];
                    if resource_name::intersect(sub_part, prefix) {
                        // if sub_part ends with "**" or "**/", keep those in remaining part
                        let remaining = if sub_part.ends_with("**/") {
                            &path_expr[i - 2..]
                        } else if sub_part.ends_with("**") {
                            &path_expr[i - 1..]
                        } else {
                            &path_expr[i + 1..]
                        };
                        // if remaining is "**" return only this since it covers all
                        if remaining == "**" {
                            return vec!["**"];
                        }
                        result.push(remaining);
                    }
                }
            }
            result
        }
    }

    async fn run(mut self) {
        // join DDS plugins group
        let group = Group::join(self.zsession.clone(), GROUP_NAME, self.member.clone()).await;
        let group_subscriber = group.subscribe().await;
        let mut group_stream = group_subscriber.stream();

        // run DDS discovery
        let (tx, rx): (Sender<DiscoveryEvent>, Receiver<DiscoveryEvent>) = unbounded();
        run_discovery(self.dp, tx);

        // declare admin space queryable
        let admin_path_prefix = format!("/@/service/{}/dds/", self.zsession.id().await);
        let admin_path_expr = format!("{}**", admin_path_prefix);
        let z = Zenoh::from(self.zsession.as_ref());
        let w = z.workspace(None).await.unwrap();
        debug!("Declare admin space on {}", admin_path_expr);
        let mut admin_space = w
            .register_eval(&PathExpr::try_from(admin_path_expr.clone()).unwrap())
            .await
            .unwrap();

        // add plugin's config and version in admin space
        self.admin_space
            .insert("config".to_string(), AdminRef::Config);
        self.admin_space
            .insert("version".to_string(), AdminRef::Version);

        let scope = self.scope.clone();
        loop {
            select!(
                evt = rx.recv().fuse() => {
                    match evt.unwrap() {
                        DiscoveryEvent::DiscoveredPublication {
                            mut entity
                        } => {
                            debug!("Discovered DDS Writer on {}: {}", entity.topic_name, entity.key);
                            // get its admin_path
                            let admin_path = DdsPlugin::get_admin_path(&entity, true);
                            let full_admin_path = format!("{}/{}", admin_path_prefix, admin_path);

                            // copy and adapt Writer's QoS for creation of a matching Reader
                            let mut qos = entity.qos.clone();
                            qos.ignore_local_participant = true;
                            // set history to KEEP_LAST 0 (no need to keep history since all is transfered to zenoh)
                            qos.history.kind = HistoryKind::KEEP_LAST;
                            qos.history.depth = 0;

                            // create 1 route per partition, or just 1 if no partition
                            if entity.qos.partitions.is_empty() {
                                let zkey = format!("{}/{}", scope, entity.topic_name);
                                let route_status = self.try_add_route_from_dds(&zkey, &full_admin_path, &entity.topic_name, &entity.type_name, entity.keyless, qos).await;
                                if let RouteStatus::Routed(ref route_key) = route_status {
                                    // if route has been created, add this Writer in its routed_writers list
                                    if let Some(r) = self.routes_from_dds.get_mut(route_key) { r.routed_writers.push(entity.key.clone()) }
                                }
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in &entity.qos.partitions {
                                    let zkey = format!("{}/{}/{}", scope, p, entity.topic_name);
                                    let route_status = self.try_add_route_from_dds(&zkey, &full_admin_path, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone()).await;
                                    if let RouteStatus::Routed(ref route_key) = route_status {
                                        // if route has been created, add this Writer in its routed_writers list
                                        if let Some(r) = self.routes_from_dds.get_mut(route_key) { r.routed_writers.push(entity.key.clone()) }
                                    }
                                    entity.routes.insert(p.clone(), route_status);
                                }
                            }

                            // store the writer
                            self.insert_dds_writer(admin_path, entity);
                        }

                        DiscoveryEvent::UndiscoveredPublication {
                            key,
                        } => {
                            debug!("Undiscovered DDS Writer {}", key);
                            self.remove_dds_writer(&key);
                        }

                        DiscoveryEvent::DiscoveredSubscription {
                            mut entity
                        } => {
                            debug!("Discovered DDS Reader on {}: {}", entity.topic_name, entity.key);
                            let admin_path = DdsPlugin::get_admin_path(&entity, false);
                            let full_admin_path = format!("{}{}", admin_path_prefix, admin_path);

                            // copy and adapt Reader's QoS for creation of a matching Writer
                            let mut qos = entity.qos.clone();
                            qos.ignore_local_participant = true;
                            // if Reader is TRANSIENT_LOCAL, configure durability_service QoS with same history than the Reader.
                            // This is because CycloneDDS is actually usinf durability_service.history for transient_local historical data.
                            if qos.durability.kind == DurabilityKind::TRANSIENT_LOCAL {
                                qos.durability_service.service_cleanup_delay = 60 * DDS_1S_DURATION;
                                qos.durability_service.history_kind = qos.history.kind;
                                qos.durability_service.history_depth = qos.history.depth;
                                qos.durability_service.max_samples = DDS_LENGTH_UNLIMITED;
                                qos.durability_service.max_instances = DDS_LENGTH_UNLIMITED;
                                qos.durability_service.max_samples_per_instance = DDS_LENGTH_UNLIMITED;
                            }
                            // Workaround for the DDS Writer to correctly match with a FastRTPS Reader declaring a Reliability max_blocking_time < infinite
                            if qos.reliability.max_blocking_time < DDS_INFINITE_TIME {
                                qos.reliability.max_blocking_time += 1;
                            }

                            // create 1 route per partition, or just 1 if no partition
                            if entity.qos.partitions.is_empty() {
                                let zkey = format!("{}/{}", scope, entity.topic_name);
                                let route_status = self.try_add_route_to_dds(&zkey, &full_admin_path, &entity.topic_name, &entity.type_name, entity.keyless, qos).await;
                                if let RouteStatus::Routed(ref route_key) = route_status {
                                    // if route has been created, add this Reader in its routed_readers list
                                    if let Some(r) = self.routes_to_dds.get_mut(route_key) { r.routed_readers.push(entity.key.clone()) }
                                }
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in &entity.qos.partitions {
                                    let zkey = format!("{}/{}/{}", scope, p, entity.topic_name);
                                    let route_status = self.try_add_route_to_dds(&zkey, &full_admin_path, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone()).await;
                                    if let RouteStatus::Routed(ref route_key) = route_status {
                                        // if route has been created, add this Reader in its routed_readers list
                                        if let Some(r) = self.routes_to_dds.get_mut(route_key) { r.routed_readers.push(entity.key.clone()) }
                                    }
                                    entity.routes.insert(p.clone(), route_status);
                                }
                            }

                            // store the reader
                            self.insert_dds_reader(admin_path, entity);
                        }

                        DiscoveryEvent::UndiscoveredSubscription {
                            key,
                        } => {
                            debug!("Undiscovered DDS Reader {}", key);
                            self.remove_dds_reader(&key)
                        }
                    }
                },

                group_event = group_stream.next().fuse() => {
                    if let Some(GroupEvent::Join(JoinEvent{member})) = group_event {
                        debug!("New zenoh_dds_plugin detected: {}", member.id());
                        // make all QueryingSubscriber to query this new member
                        for (zkey, zsub) in &mut self.routes_to_dds {
                            if let ZSubscriber::QueryingSubscriber(sub) = &mut zsub.zenoh_subscriber {
                                let rkey: ResKey = format!("{}/{}/{}", PUB_CACHE_QUERY_PREFIX, member.id(), zkey).into();
                                debug!("Query for TRANSIENT_LOCAL topic on: {}", rkey);
                                let target = QueryTarget {
                                    kind: PUBLICATION_CACHE_QUERYABLE_KIND,
                                    target: Target::All,
                                };
                                if let Err(e) = sub.query_on(&rkey, "", target, QueryConsolidation::none()).await {
                                    warn!("Query on {} for TRANSIENT_LOCAL topic failed: {}", rkey, e);
                                }
                            }
                        }
                    }
                }

                get_request = admin_space.next().fuse() => {
                    self.treat_admin_query(get_request.unwrap(), &admin_path_prefix).await;
                }
            )
        }
    }
}

//TODO replace when stable https://github.com/rust-lang/rust/issues/65816
#[inline]
pub(crate) fn vec_into_raw_parts<T>(v: Vec<T>) -> (*mut T, usize, usize) {
    let mut me = ManuallyDrop::new(v);
    (me.as_mut_ptr(), me.len(), me.capacity())
}
