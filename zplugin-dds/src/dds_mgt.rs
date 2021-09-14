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
use crate::qos::Qos;
use async_std::channel::Sender;
use async_std::task;
use cyclors::*;
use log::debug;
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::os::raw;
use std::sync::Arc;
use zenoh::net::{ResKey, Session, ZBuf};

const MAX_SAMPLES: usize = 32;

#[derive(PartialEq, Debug, Serialize)]
pub(crate) enum RouteStatus {
    Routed(String), // Routing is active, String is the zenoh zenoh resource key used for the route
    NotAllowed,     // Routing was not allowed per configuration
    CreationFailure(String), // The route creation failed
    _QoSConflict,   // A route was already established but with conflicting QoS
}

#[derive(Debug, Serialize)]
pub(crate) struct DdsEntity {
    pub(crate) key: String,
    pub(crate) participant_key: String,
    pub(crate) topic_name: String,
    pub(crate) type_name: String,
    pub(crate) keyless: bool,
    pub(crate) qos: Qos,
    pub(crate) routes: HashMap<String, RouteStatus>, // map of routes statuses indexed by partition ("*" only if no partition)
}

#[derive(Debug)]
pub(crate) enum DiscoveryEvent {
    DiscoveredPublication { entity: DdsEntity },
    UndiscoveredPublication { key: String },
    DiscoveredSubscription { entity: DdsEntity },
    UndiscoveredSubscription { key: String },
}

unsafe extern "C" fn on_data(dr: dds_entity_t, arg: *mut std::os::raw::c_void) {
    let btx = Box::from_raw(arg as *mut (bool, Sender<DiscoveryEvent>));
    let pub_discovery: bool = btx.0;
    let dp = dds_get_participant(dr);
    let mut dpih: dds_instance_handle_t = 0;
    let _ = dds_get_instance_handle(dp, &mut dpih);

    #[allow(clippy::uninit_assumed_init)]
    let mut si: [dds_sample_info_t; MAX_SAMPLES] = { MaybeUninit::uninit().assume_init() };
    let mut samples: [*mut ::std::os::raw::c_void; MAX_SAMPLES] =
        [std::ptr::null_mut(); MAX_SAMPLES as usize];
    samples[0] = std::ptr::null_mut();

    let n = dds_take(
        dr,
        samples.as_mut_ptr() as *mut *mut raw::c_void,
        si.as_mut_ptr() as *mut dds_sample_info_t,
        MAX_SAMPLES as u64,
        MAX_SAMPLES as u32,
    );
    for i in 0..n {
        if si[i as usize].valid_data {
            let sample = samples[i as usize] as *mut dds_builtintopic_endpoint_t;
            let is_alive = si[i as usize].instance_state == dds_instance_state_DDS_IST_ALIVE;
            let key = hex::encode((*sample).key.v);
            let participant_key = hex::encode((*sample).participant_key.v);
            let keyless = (*sample).key.v[15] == 3 || (*sample).key.v[15] == 4;
            let topic_name = CStr::from_ptr((*sample).topic_name).to_str().unwrap();
            let type_name = CStr::from_ptr((*sample).type_name).to_str().unwrap();

            debug!(
                "{} DDS {} {} from Participant {} on {} with type {} (keyless: {})",
                if is_alive {
                    "Discovered"
                } else {
                    "Undiscovered"
                },
                if pub_discovery {
                    "publication"
                } else {
                    "subscription"
                },
                key,
                participant_key,
                topic_name,
                type_name,
                keyless
            );

            if topic_name.contains("DCPS") || (*sample).participant_instance_handle == dpih {
                debug!(
                    "Ignoring discovery of {} ({}) from local participant",
                    key, topic_name
                );
                continue;
            }

            // send a DiscoveryEvent
            if si[i as usize].instance_state == dds_instance_state_DDS_IST_ALIVE {
                let qos = if pub_discovery {
                    Qos::from_writer_qos_native((*sample).qos)
                } else {
                    Qos::from_reader_qos_native((*sample).qos)
                };

                let entity = DdsEntity {
                    key: key.clone(),
                    participant_key: participant_key.clone(),
                    topic_name: String::from(topic_name),
                    type_name: String::from(type_name),
                    keyless,
                    qos,
                    routes: HashMap::<String, RouteStatus>::new(),
                };

                if pub_discovery {
                    (btx.1)
                        .try_send(DiscoveryEvent::DiscoveredPublication { entity })
                        .unwrap();
                } else {
                    (btx.1)
                        .try_send(DiscoveryEvent::DiscoveredSubscription { entity })
                        .unwrap();
                }
            } else if pub_discovery {
                (btx.1)
                    .try_send(DiscoveryEvent::UndiscoveredPublication { key })
                    .unwrap();
            } else {
                (btx.1)
                    .try_send(DiscoveryEvent::UndiscoveredSubscription { key })
                    .unwrap();
            }
        }
    }
    dds_return_loan(
        dr,
        samples.as_mut_ptr() as *mut *mut raw::c_void,
        MAX_SAMPLES as i32,
    );
    Box::into_raw(btx);
}

pub(crate) fn run_discovery(dp: dds_entity_t, tx: Sender<DiscoveryEvent>) {
    unsafe {
        let ptx = Box::new((true, tx.clone()));
        let stx = Box::new((false, tx));
        let sub_listener = dds_create_listener(Box::into_raw(ptx) as *mut std::os::raw::c_void);
        dds_lset_data_available(sub_listener, Some(on_data));

        let _pr = dds_create_reader(
            dp,
            DDS_BUILTIN_TOPIC_DCPSPUBLICATION,
            std::ptr::null(),
            sub_listener,
        );

        let sub_listener = dds_create_listener(Box::into_raw(stx) as *mut std::os::raw::c_void);
        dds_lset_data_available(sub_listener, Some(on_data));
        let _sr = dds_create_reader(
            dp,
            DDS_BUILTIN_TOPIC_DCPSSUBSCRIPTION,
            std::ptr::null(),
            sub_listener,
        );
    }
}

unsafe extern "C" fn data_forwarder_listener(dr: dds_entity_t, arg: *mut std::os::raw::c_void) {
    let pa = arg as *mut (ResKey, Arc<Session>);
    let mut zp: *mut cdds_ddsi_payload = std::ptr::null_mut();
    #[allow(clippy::uninit_assumed_init)]
    let mut si: [dds_sample_info_t; 1] = { MaybeUninit::uninit().assume_init() };
    while cdds_take_blob(dr, &mut zp, si.as_mut_ptr()) > 0 {
        if si[0].valid_data {
            log::trace!("Route data to zenoh resource with rid={}", &(*pa).0);
            let bs = Vec::from_raw_parts((*zp).payload, (*zp).size as usize, (*zp).size as usize);
            let rbuf = ZBuf::from(bs);
            let _ = task::block_on(async { (*pa).1.write(&(*pa).0, rbuf).await });
            (*zp).payload = std::ptr::null_mut();
        }
        cdds_serdata_unref(zp as *mut ddsi_serdata);
    }
}

pub fn create_forwarding_dds_reader(
    dp: dds_entity_t,
    topic_name: String,
    type_name: String,
    keyless: bool,
    qos: Qos,
    z_key: ResKey,
    z: Arc<Session>,
) -> Result<dds_entity_t, String> {
    let cton = CString::new(topic_name).unwrap().into_raw();
    let ctyn = CString::new(type_name).unwrap().into_raw();

    unsafe {
        let t = cdds_create_blob_topic(dp, cton, ctyn, keyless);
        let arg = Box::new((z_key, z));
        let sub_listener = dds_create_listener(Box::into_raw(arg) as *mut std::os::raw::c_void);
        dds_lset_data_available(sub_listener, Some(data_forwarder_listener));
        let qos_native = qos.to_qos_native();
        let reader = dds_create_reader(dp, t, qos_native, sub_listener);
        Qos::delete_qos_native(qos_native);
        if reader >= 0 {
            let res = dds_reader_wait_for_historical_data(reader, crate::qos::DDS_100MS_DURATION);
            if res < 0 {
                log::error!(
                    "Error caling dds_reader_wait_for_historical_data(): {}",
                    CStr::from_ptr(dds_strretcode(-res)).to_str().unwrap()
                );
            }
            Ok(reader)
        } else {
            Err(format!(
                "Error creating DDS Reader: {}",
                CStr::from_ptr(dds_strretcode(-reader)).to_str().unwrap()
            ))
        }
    }
}

pub fn create_forwarding_dds_writer(
    dp: dds_entity_t,
    topic_name: String,
    type_name: String,
    keyless: bool,
    qos: Qos,
) -> Result<dds_entity_t, String> {
    let cton = CString::new(topic_name).unwrap().into_raw();
    let ctyn = CString::new(type_name).unwrap().into_raw();

    unsafe {
        let t = cdds_create_blob_topic(dp, cton, ctyn, keyless);
        let qos_native = qos.to_qos_native();
        let writer = dds_create_writer(dp, t, qos_native, std::ptr::null_mut());
        Qos::delete_qos_native(qos_native);
        if writer >= 0 {
            Ok(writer)
        } else {
            Err(format!(
                "Error creating DDS Writer: {}",
                CStr::from_ptr(dds_strretcode(-writer)).to_str().unwrap()
            ))
        }
    }
}

pub fn delete_dds_entity(entity: dds_entity_t) -> Result<(), String> {
    unsafe {
        let r = dds_delete(entity);
        match r {
            0 | DDS_RETCODE_ALREADY_DELETED => Ok(()),
            e => Err(format!("Error deleting DDS entity - retcode={}", e)),
        }
    }
}
