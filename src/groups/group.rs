use std::{str::FromStr, path::PathBuf};

use nostr::{EventId, secp256k1::XOnlyPublicKey, prelude::UncheckedUrl, Tag, Event};
use nostr_sdk::{Timestamp, Keys};

use crate::{utils::load_event, ngit_tag::{tag_extract_value_as_event_id, tag_extract_relays, tag_is_group, tag_group_with_relays}};

use super::{init::{InitializeGroup, self}};

/// [`Group`] error
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error processing initialisation group content json - incorrect format?
    #[error("group cannot be initialised from content: {0}")]
    InitializeJson(#[from] init::Error),
    /// Error group is not avialable locally in .ngit
    #[error("group is not available in .ngit/groups/{0}.json")]
    GroupJsonNotAvailable(String),
}

#[derive(Eq, PartialEq, Clone)]
pub struct StartFinish {
    pub start:Timestamp,
    pub finish: Option<Timestamp>,
}

struct MembershipDetails {
    pub id:EventId,
    pub dates: Vec<StartFinish>,
    pub relays: Vec<UncheckedUrl>,
}
pub struct MembershipCollection {
    collection:Vec<MembershipDetails>,
}
impl MembershipCollection {
    pub fn new() -> Self {
        Self {
            collection: vec![],
        }
    }
    pub fn add_group_dates(
        &mut self,
        group_tag:Tag,
        start_finish:StartFinish,
    ) {
        
        if !tag_is_group(&group_tag) {
            panic!("tag supplied to add_group_dates isn't a group tag");
        }

        match self.collection.iter_mut().find(
            |g| tag_extract_value_as_event_id(&group_tag).eq(&g.id)
        ) {
            None => {
                self.collection.push(
                    MembershipDetails { 
                        id: tag_extract_value_as_event_id(&group_tag),
                        dates: vec![
                            start_finish
                        ],
                        relays: tag_extract_relays(&group_tag),
                    }
                );
            },
            Some(group_dates_relays) => {
                match group_dates_relays.dates.iter().find(
                    |d| start_finish.eq(&d)
                ) {
                    None => group_dates_relays.dates.push(start_finish),
                    Some(_) => (),
                }
            }
        }    
    }

    pub fn get_first_active_group(&self) -> Option<&EventId> {
        let a = self.get_active_groups();
        if a.is_empty() { None }
        else { Some(a[0]) }
    }

    pub fn get_active_groups(&self) -> Vec<&EventId> {
        let mut active = vec![];
        for m in &self.collection {
            if m.dates.iter().any(|sf| sf.finish.is_none()) {
                active.push(&m.id);
            }
        }
        active
    }
}

pub struct PubKeyDates {
    pubkey:XOnlyPublicKey,
    dates: Vec<StartFinish>,
}

pub struct Group {
    pub id: EventId,
    name:Option<String>,
    about:Option<String>,
    picture:Option<String>,
    pub relays:Vec<String>,
    direct_members: Vec<PubKeyDates>,
    member_groups:MembershipCollection,
    indirect_member_groups:MembershipCollection,
    admin_group:MembershipCollection,
    indirect_admin_groups:MembershipCollection,
    pub events:Vec<Event>,
    hash: String, // hash of event IDs that make up this state
}

impl Group {

    pub fn new(init:&InitializeGroup, keys:&Keys) -> Result<Self,Error> {
        let event = init.initialize(&keys);
        Group::new_from_event(event)
    }

    pub fn new_from_json_event(json_string:String) -> Result<Self,Error> {
        let event = Event::from_json(json_string)
            .expect("json_string to be formated as event");
        Group::new_from_event(event)
    }

    pub fn open (group_id:String, repo_dir_path:&PathBuf) -> Result<Self,Error> {
        let path = repo_dir_path.join(
            format!(".ngit/groups/{}.json",group_id)
        );
        if path.exists() {
            Ok(
                Group::new_from_event(
                    load_event(path)
                        .expect("group event in json to be well formatted as a group event"),
                )
                    .expect("file content at path to be a well formated group event")
            )
        }
        else {
            Err(Error::GroupJsonNotAvailable(group_id))
        }
    }

    pub fn new_from_event(event:Event) -> Result<Self,Error> {
        match InitializeGroup::from_json(&event.content) {
            Err(e) => return Err(Error::InitializeJson(e)),
            Ok(g) => {
                let start_finish = StartFinish { start: event.created_at, finish: None };
                let mut direct_members: Vec<PubKeyDates> = vec![];
                // add direct_members
                for m in g.direct_members {
                    let key = XOnlyPublicKey::from_str(m.as_str());
                    match key {
                        Ok(k) => direct_members.push(
                            PubKeyDates { 
                                pubkey: k, 
                                dates: vec![
                                    start_finish.clone(),
                                ]
                            },
                        ),
                        // could add pubkey to an invalid vector and report on it?
                        Err(_) => (), 
                    }
                }
                // add member groups
                // let mut member_groups: Vec<GroupDatesRelays> = vec![];
                let mut member_groups = MembershipCollection::new();
                for m in g.member_groups {
                    // let event_id_relay = EventIdRelays::from_tag(m);
                    // member_groups.push(GroupDatesRelays { 
                    //     id: event_id_relay.id,
                    //     dates: vec![
                    //         StartFinish { start: event.created_at, finish: None }
                    //     ],
                    //     relays: match event_id_relay.relay {
                    //         None => vec![],
                    //         Some(r) => vec![r],
                    //     }
                    // });
                    // add_group_dates_to_vector(
                    //     &mut member_groups,
                    //     m,
                    //     StartFinish { start: event.created_at, finish: None },
                    // )
                    member_groups.add_group_dates(
                        m,
                        start_finish.clone(),
                    )
                }
                // add admin group
                // let admin_group = match g.admin {
                //     None => vec![],
                //     Some(a) => match EventId::from_str(a.as_str()) {
                //         Ok(id) => vec![
                //             GroupDatesRelays {
                //                 id,
                //                 dates: vec![StartFinish {
                //                     start: event.created_at.clone(),
                //                     finish: None,
                //                 }]
                //             }
                //         ],
                //         // could report on it?
                //         Err(_) => vec![], 
                //     }
                // };
                let mut admin_group = MembershipCollection::new();
                // let admin_group = vec![];
                match g.admin {
                    None => (),
                    Some(t) => {
                        admin_group.add_group_dates(
                            t,
                            start_finish.clone(),
                        )
                    }
                }
                Ok(Self {
                    id: event.id,
                    name: g.name,
                    about: g.about,
                    picture: g.picture,
                    relays: g.relays,
                    direct_members,
                    admin_group,
                    member_groups,
                    indirect_member_groups: MembershipCollection::new(),
                    indirect_admin_groups: MembershipCollection::new(),
                    events:vec![event],
                    hash: "hash".to_string(), // hash of event IDs that make up this state
                })
            }
        }        
    }

    fn load_recurring_sub_groups() {

    }

    pub fn get_ref(&self) -> Tag {
        tag_group_with_relays(
            &self.id.to_string(),
            &self.relays,
        )
    }

    // fn add_member(&self,) -> Self;
    // fn init(&self,keys:Keys) -> Event;
    // fn remove_member() -> Self;
    // fn set_admin(&self) -> Self;
    // fn set_name(&self) -> Self;
    // fn set_about(&self) -> Self;
    // fn set_picture(&self) -> Self;

    // use enums instead of having so many functions? then use a vector to store all the changes so they can be made in one event?

    // fn new(
    //     &self,
    //     direct_members:Vec<String>,
    //     sub_groups:String,
    //     relays:Vec<String>,
    //     name:String,
    // ) -> Self {
    //     // create initialation event
    //     // EventBuilder::new(
    //     //     100,

    //     // )
    //     self
    // }

    pub fn members(&self) -> Vec<&XOnlyPublicKey> {
        let mut pubkeys = vec![];
        for m in &self.direct_members {
            pubkeys.push(&m.pubkey);
        }
        pubkeys
    }

    pub fn is_member(&self, pubkey: &XOnlyPublicKey) -> bool {
        self.members().iter().any(|m| *pubkey == **m)
    }
    // pub fn admins(&self) -> Vec<&String> { get_el(&self.admins) }
    // pub fn voters(&self) -> Vec<&String> { get_el(&self.voters) }
    // pub fn members(&self) -> Vec<&String> { get_el(&self.members) }
    // pub fn is_admin(&self, pubkey:&String) -> bool { is_el(&self.admins, &pubkey) }
    // pub fn is_voter(&self, pubkey:&String) -> bool { is_el(&self.voters, &pubkey) }
    // pub fn is_member(&self, pubkey:&String) -> bool { is_el(&self.members, &pubkey) }
    // pub fn get_admins_at<'a>(&'a self, timestamp: &'a Timestamp) -> Vec<&String> { get_el_at(&self.admins,&timestamp) }
    // pub fn get_voters_at<'a>(&'a self, timestamp: &'a Timestamp) -> Vec<&String> { get_el_at(&self.voters,&timestamp) }
    // pub fn get_members_at<'a>(&'a self, timestamp: &'a Timestamp) -> Vec<&String> { get_el_at(&self.members,&timestamp) }
    // pub fn was_admin_at(&self, pubkey:&String, timestamp: &Timestamp) -> bool { was_el_at(&self.admins, pubkey, &timestamp) }
    // pub fn was_voter_at(&self, pubkey:&String, timestamp: &Timestamp) -> bool { was_el_at(&self.voters, pubkey, &timestamp) }
    // pub fn was_members_at(&self, pubkey:&String, timestamp: &Timestamp) -> bool { was_el_at(&self.members, pubkey, &timestamp) }
}

// fn get_el(el:&Vec<PubKeyDates>) -> Vec<&String> {
//     let mut current: Vec<&String> = vec![];
//     for m in el {
//         if m.dates.last().unwrap().finish.is_none() {
//             current.push(&m.pubkey)
//         }
//     }
//     current
// }
// fn is_el(el:&Vec<PubKeyDates>, pubkey:&String) -> bool {
//     el
//         .iter()
//         .any(
//             |m|
//             &m.pubkey == pubkey
//             && m.dates.last().unwrap().finish.is_none()
//         )
// }
// fn get_el_at<'a>(el:&'a Vec<PubKeyDates>, timestamp: &'a Timestamp) -> Vec<&'a String> {
//     let mut el_at_timestamp: Vec<&String> = vec![];
//     for m in el {
//         if m.dates
//             .iter()
//             .any(
//                 |d|
//                 &d.start < &timestamp
//                 // && match &d.finish {
//                 //     None => true,
//                 //     _ => &d.finish.unwrap() > &timestamp
//                 // }
//                 && (
//                     d.finish.is_none()
//                     || &d.finish.unwrap() > &timestamp
//                 )
//             ) {
//                 el_at_timestamp.push(&m.pubkey)
//             }
//     }
//     el_at_timestamp
// }
// fn was_el_at(el:&Vec<PubKeyDates>, pubkey:&String,timestamp:&Timestamp) -> bool {
//     // PublicKey::try_from_hex_string(pubkey);
//     el
//         .iter()
//         .any(
//             |m|
//             &m.pubkey == pubkey
//             && m.dates
//                 .iter()
//                 .any(
//                     |d|
//                     &d.start < &timestamp
//                     && (
//                         d.finish.is_none()
//                         || &d.finish.unwrap() > &timestamp
//                     )
//                 )
//         )
// }

