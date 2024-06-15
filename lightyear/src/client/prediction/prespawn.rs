//! Handles spawning entities that are predicted

use std::any::TypeId;
use std::hash::{Hash, Hasher};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::client::components::Confirmed;
use crate::client::connection::ConnectionManager;
use crate::client::events::ComponentInsertEvent;
use crate::client::prediction::resource::PredictionManager;
use crate::client::prediction::rollback::Rollback;
use crate::client::prediction::Predicted;
use crate::client::replication::send::ReplicateToServer;
use crate::prelude::client::PredictionSet;
use crate::prelude::server::ControlledBy;
use crate::prelude::{
    ComponentRegistry, NetworkRelevanceMode, ParentSync, ReplicateHierarchy, Replicated,
    Replicating, ReplicationTarget, ShouldBePredicted, TargetEntity, TickManager,
};
use crate::protocol::component::ComponentKind;
use crate::server::relevance::immediate::CachedNetworkRelevance;
use crate::server::replication::send::SyncTarget;
use crate::shared::replication::components::DespawnTracker;
use crate::shared::sets::{ClientMarker, InternalReplicationSet};

#[derive(Default)]
pub(crate) struct PreSpawnedPlayerObjectPlugin;

#[derive(SystemSet, Debug, Hash, PartialEq, Eq, Clone, Copy)]
pub enum PreSpawnedPlayerObjectSet {
    // PreUpdate Sets
    /// When we receive an entity from the server that contains the [`PreSpawnedPlayerObject`] component,
    /// that means that it was already spawned on the client.
    /// Do the matching process to find the corresponding client entity
    Spawn,
    // PostUpdate Sets
    /// Add the necessary information to the PrePrediction component (before replication)
    /// Clean up the PreSpawnedPlayerObject entities for which we couldn't find a mapped server entity
    CleanUp,
}

impl Plugin for PreSpawnedPlayerObjectPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            PreUpdate,
            PreSpawnedPlayerObjectSet::Spawn.in_set(PredictionSet::SpawnPrediction),
        );
        app.configure_sets(
            PostUpdate,
            PreSpawnedPlayerObjectSet::CleanUp.in_set(PredictionSet::All),
        );
        app.configure_sets(
            FixedPostUpdate,
            // we run the prespawn hash at FixedUpdate AND PostUpdate (to handle entities spawned during Update)
            // TODO: entities spawned during update might have a tick that is off by 1 or more...
            //  account for this when setting the hash?
            // NOTE: we need to call this before SpawnHistory otherwise the history would affect the hash.
            // TODO: find a way to exclude predicted history from the hash
            InternalReplicationSet::<ClientMarker>::SetPreSpawnedHash
                .in_set(PredictionSet::All)
                .before(PredictionSet::SpawnHistory),
        );

        app.add_systems(
            PreUpdate,
            // we first try to see if the entity was a PreSpawnedPlayerObject
            // if we couldn't match it then the component gets removed and then should we try the normal spawn-prediction flow
            // TODO: or should we just consider that there was an error, and not go through the normal prediction flow?
            (Self::match_with_received_server_entity, apply_deferred)
                .chain()
                .in_set(PreSpawnedPlayerObjectSet::Spawn),
        );
        app.add_systems(
            FixedPostUpdate,
            // compute hashes for all pre-spawned player objects
            Self::compute_prespawn_hash
                .in_set(InternalReplicationSet::<ClientMarker>::SetPreSpawnedHash),
        );

        app.add_systems(
            PostUpdate,
            (
                Self::pre_spawned_player_object_cleanup.in_set(PreSpawnedPlayerObjectSet::CleanUp),
                // TODO: right now we only support pre-spawning during FixedUpdate::Main because we need the exact
                //  tick to compute the hash
                // compute hashes for all pre-spawned player objects
                // Self::compute_prespawn_hash
                //     .in_set(InternalReplicationSet::<ClientMarker>::SetPreSpawnedHash),
            ),
        );
    }
}

impl PreSpawnedPlayerObjectPlugin {
    /// Compute the hash of the prespawned entity by hashing the type of all its components along with the tick at which it was created
    pub(crate) fn compute_prespawn_hash(world: &mut World) {
        // get the rollback tick if the pre-spawned entity is being recreated during rollback!
        let tick = world
            .resource::<TickManager>()
            .tick_or_rollback_tick(world.resource::<Rollback>());

        world.resource_scope(|world: &mut World, mut manager: Mut<PredictionManager>| {
            world.resource_scope(
                |world: &mut World, component_registry: Mut<ComponentRegistry>| {
                    let components = world.components();

                    // ignore replicated entities, we only want to iterate through entities
                    // spawned on the client
                    let mut pre_spawned_query = world
                .query_filtered::<(EntityRef, Ref<PreSpawnedPlayerObject>), (Without<Replicated>, Without<Confirmed>)>();
                    // let mut predicted_entities = vec![];
                    for (entity_ref, prespawn) in pre_spawned_query.iter(world) {
                        // we only care about newly-added PreSpawnedPlayerObject components
                        if !prespawn.is_added() {
                            continue;
                        }
                        let entity = entity_ref.id();
                        let hash = prespawn.hash.map_or_else(|| {
                            // TODO: try EntityHasher instead since we only hash the 64 lower bits of TypeId
                            // TODO: should I create the hasher once outside?

                            // NOTE: tried
                            // - bevy::utils::RandomState::with_seeds(1, 2, 3, 4).build_hasher();
                            // - xxhash_rust::xxh3::Xxh3Builder::new().with_seed(1).build_hasher();
                            // - bevy::utils::AHasher::default();
                            // but they were not deterministic across processes
                            let mut hasher = seahash::SeaHasher::new();

                            // TODO: this only works currently for entities that are spawned during Update!
                            //  if we want the tick to be valid, compute_hash should also be run at the end of FixedUpdate::Main
                            //  so that we have the exact spawn tick! Solutions:
                            //  run compute_hash in post-update as well
                            // we include the spawn tick in the hash
                            tick.hash(&mut hasher);
                            //
                            // // TODO: we only want to use components from the protocol, because server/client might use a lot of different stuff...
                            // entity_ref.contains_type_id()

                            // NOTE: we cannot call hash() multiple times because the components in the archetype
                            //  might get iterated in any order!
                            //  Instead we will get the sorted list of types to hash first, sorted by type_id
                            let mut kinds_to_hash = entity_ref
                                .archetype()
                                .components()
                                .filter_map(|component_id| {
                                    if let Some(type_id) =
                                        world.components().get_info(component_id).unwrap().type_id()
                                    {
                                        // ignore some book-keeping components
                                        if type_id != TypeId::of::<NetworkRelevanceMode>()
                                            && type_id != TypeId::of::<ReplicationTarget>()
                                            && type_id != TypeId::of::<SyncTarget>()
                                            && type_id != TypeId::of::<ControlledBy>()
                                            && type_id != TypeId::of::<Replicating>()
                                            && type_id != TypeId::of::<Replicated>()
                                            && type_id != TypeId::of::<ReplicateToServer>()
                                            && type_id != TypeId::of::<CachedNetworkRelevance>()
                                            && type_id != TypeId::of::<NetworkRelevanceMode>()
                                            && type_id != TypeId::of::<TargetEntity>()
                                            && type_id != TypeId::of::<ReplicateHierarchy>()
                                            && type_id != TypeId::of::<PreSpawnedPlayerObject>()
                                            && type_id != TypeId::of::<ShouldBePredicted>()
                                            && type_id != TypeId::of::<DespawnTracker>()
                                            && type_id != TypeId::of::<ParentSync>()
                                        {
                                            return component_registry.kind_map.net_id(&ComponentKind::from(type_id)).copied();
                                        }
                                    }
                                    None
                                })
                                .collect::<Vec<_>>();
                            kinds_to_hash.sort();
                            kinds_to_hash.into_iter().for_each(|kind| {
                                trace!(?kind, "using kind for hash");
                                kind.hash(&mut hasher)
                            });

                            // No need to set the value on the component here, we only need the value in the resource!
                            // prespawn.hash = Some(hasher.finish());

                            let new_hash = hasher.finish();
                            debug!(?entity, ?tick, hash = ?new_hash, "computed spawn hash for entity");
                            new_hash
                        },
                        |hash| {
                            trace!(
                                ?entity,
                                ?tick,
                                ?hash,
                                "the hash has already been computed for the entity!"
                            );
                            hash
                        });

                        // check if we can match with an existing server entity that was received
                        // before the client entity was spawned
                        // this could happen if we are predicting remote players:
                        // - client 1 presses input and spawns a prespawned-object
                        // - the pre-spawned object AND the input are replicated to player 2
                        // - player 2 receives BOTH the replicated object and the input, and spawns a duplicate object


                        // TODO: what to do in multiple entities share the same hash?
                        //  just match a random one of them? or should the user have a more precise hash?
                        manager
                            .prespawn_hash_to_entities
                            .entry(hash)
                            .or_default()
                            .push(entity);
                        // add a timer on the entity so that it gets despawned if the interpolation tick
                        // reaches it without matching with any server entity
                        manager.prespawn_tick_to_hash.push(tick, hash);
                        // predicted_entities.push(entity);
                    }

                    // NOTE: originally I wanted to remove PreSpawnedPlayerObject here because I wanted to call `compute_hash`
                    // at PostUpdate, which would run twice (at the end of FixedUpdate and at PostUpdate)
                    // But actually we need the component to be present so that we spawn a ComponentHistory

                    // for entity in predicted_entities {
                    //     info!("remove PreSpawnedPlayerObject");
                    //     // we stored the relevant information in the PredictionManager resource
                    //     // so we can remove the component here
                    //     world.entity_mut(entity).remove::<PreSpawnedPlayerObject>();
                    // }
                },
            );
        });
    }

    // TODO: should we require that ShouldBePredicted is present on the entity?
    /// When we receive an entity from the server that contains the PreSpawnedPlayerObject component,
    /// that means that we already spawned it on the client.
    /// Try to match which client entity it is and take authority over it.
    pub(crate) fn match_with_received_server_entity(
        mut commands: Commands,
        connection: Res<ConnectionManager>,
        mut manager: ResMut<PredictionManager>,
        // TODO: replace with Query<&PreSpawnedPlayerObject, Added<Replicating>> ?
        mut events: EventReader<ComponentInsertEvent<PreSpawnedPlayerObject>>,
        query: Query<&PreSpawnedPlayerObject>,
    ) {
        // ComponentInsertEvent is emitted by replication systems, so server has replicated us an entity
        // with a PreSpawnedPlayerObject component.
        for event in events.read() {
            let confirmed_entity = event.entity();
            let confirmed_tick = connection
                .replication_receiver
                .get_confirmed_tick(confirmed_entity)
                .unwrap();

            let server_prespawn = query.get(confirmed_entity).unwrap();
            let Some(server_hash) = server_prespawn.hash else {
                warn!("Received a PreSpawnedPlayerObject entity from the server without a hash");
                continue;
            };

            // Find or spawn the Predicted entity.

            let predicted_entity = manager
                // is there a prespawned entity matching this hash?
                .take_entity_for_prespawn_hash(&server_hash)
                // if it exists, update components accordingly
                .and_then(|e| {
                    commands
                        .get_entity(e)
                        .map(|mut entity_commands| {
                            warn!("re-using existing entity, hash: {server_hash}, confirmed_tick: {confirmed_tick:?}");
                            entity_commands
                                .remove::<PreSpawnedPlayerObject>()
                                .insert(Predicted {
                                    confirmed_entity: Some(confirmed_entity),
                                });
                            e
                        })
                        .or_else(|| {
                            warn!(?server_hash, "Received a PreSpawnedPlayerObject entity from the server with a hash that does not match any client entity");
                            None
                        })
                })
                // if no such entity found, spawn one
                .or_else(|| {
                    warn!("spawning new entity, hash: {server_hash}, confirmed_tick: {confirmed_tick:?}");
                    Some(
                        commands
                            .spawn(Predicted {
                                confirmed_entity: Some(confirmed_entity),
                            })
                            .id(),
                    )
                })
                .unwrap();

            // Assign Confirmed to the server entity's counterpart, and remove PreSpawnedPlayerObject
            commands
                .entity(confirmed_entity)
                .insert(Confirmed {
                    predicted: Some(predicted_entity),
                    interpolated: None,
                    tick: confirmed_tick,
                })
                // remove ShouldBePredicted so that we don't spawn another Predicted entity
                .remove::<(PreSpawnedPlayerObject, ShouldBePredicted)>();

            warn!(
                "Added/Spawned the Predicted entity: {:?} for the confirmed entity: {:?} (confirmed_tick: {confirmed_tick:?}, hash: {server_hash})",
                predicted_entity, confirmed_entity
            );
        }
    }

    /// Cleanup the client prespawned entities for which we couldn't find a mapped server entity
    pub(crate) fn pre_spawned_player_object_cleanup(
        mut commands: Commands,
        tick_manager: Res<TickManager>,
        connection: Res<ConnectionManager>,
        mut manager: ResMut<PredictionManager>,
    ) {
        let tick = tick_manager.tick();
        // TODO: why is interpolation tick not good enough and we need to use an earlier tick?
        // TODO: for some reason at interpolation_tick we often haven't received the update from the server yet!
        //  use a tick that it's even more in the past
        let interpolation_tick = connection.sync_manager.interpolation_tick(&tick_manager);
        trace!(
            ?tick,
            ?interpolation_tick,
            "cleaning up prespawned player objects"
        );
        // NOTE: cannot assert because of tick_wrap tests
        // assert!(
        //     tick >= interpolation_tick,
        //     "tick {:?} should be greater than interpolation_tick {:?}",
        //     tick,
        //     interpolation_tick
        // );
        let tick_diff = (tick - interpolation_tick).saturating_mul(2) as u16;
        let past_tick = tick - tick_diff;
        // remove all the prespawned entities that have not been matched with a server entity
        for (_, hash) in manager.prespawn_tick_to_hash.drain_until(&past_tick) {
            manager
                .prespawn_hash_to_entities
                .remove(&hash)
                .iter()
                .flatten()
                .for_each(|entity| {
                    if let Some(entity_commands) = commands.get_entity(*entity) {
                        warn!(
                            ?tick,
                            ?entity,
                            "Cleaning up prespawned player object up to past tick: {:?}",
                            past_tick
                        );
                        entity_commands.despawn_recursive();
                    }
                });
        }
    }
}

#[derive(
    Component, Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq, Reflect,
)]
#[component(storage = "SparseSet")]
pub struct PreSpawnedPlayerObject {
    /// The hash that will identify the spawned entity
    /// By default, if the hash is not set, it will be generated from the entity's archetype (list of components) and spawn tick
    /// Otherwise you can manually set it to a value that will be the same on both the client and server
    pub hash: Option<u64>,
    //
    // pub conflict_resolution: ConflictResolution,
}

// pub enum ClientNoMatchHandling {
//     /// If we don't get any server-entity that matches this prespawned player object, then we despawn it on the client
//     /// Once we are sure that we won't get any more server updates for that entity
//     /// (i.e. once interpolation_tick is reached)
//     Despawn,
//
//     /// Even if we don't get any server-entity that matches this prespawned player object, we don't bother despawning it
//     /// and we just leave it as is
//     Allow,
// }
//
// pub enum ServerNoMatchHandling {
//     /// If the server sends an entity that doesn't match any existing client prespawned player object, we consider that the server
//     /// entity is still valid and we spawn a Predicted entity for it.
//     ForcePrediction,
// }

// pub enum ConflictResolution {
//     /// If we don't get any server-entity that matches this prespawned player object, then we despawn it on the client
//     /// Once we are sure that we won't get any more server updates for that entity
//     /// (i.e. once interpolation_tick is reached)
//     DespawnClient,
//     /// If the server sends us an entity that doesn't match any client prespawned player object, we consider that the entity
//     /// should still be predicted normally.
//     AllowDuplicate,
// }

// TODO: maybe provide a prediction_spawn command instead of running the `compute_hash` in both FixedUpdate and PostUpdate?

// /// This command must be used to spawn predicted entities
// /// - It will insert the
// /// - If the entity is confirmed, we despawn both the predicted and confirmed entities
// pub struct PredictionSpawnCommand {
//     entity: Entity,
//     _marker: PhantomData,
// }
//
// impl Command for PredictionSpawnCommand {
//     fn apply(self, world: &mut World) {
//         todo!()
//     }
// }
//
// pub trait PredictionSpawnCommandsExt {
//     fn prediction_spawn(&mut self);
//
// }
// impl PredictionSpawnCommandsExt for EntityCommands<'_, '_, '_> {
//     fn prediction_spawn(&mut self, pre) {
//         let entity = self.id();
//         self.commands().add(PredictionDespawnCommand {
//             entity,
//             _marker: PhantomData::,
//         })
//     }
// }

// At the end of Update, maintain a HashMap from hash -> entity for the client-side pre-spawned entities
// when we get a server entity with PreSpawned

// pub enum PredictedMode {
//     /// The entity is spawned on the server and then replicated to the client, which will spawn a Confirmed and a Predicted entity
//     FromServer,
//     /// The entity is spawned on the client, which will send a message to the server to tell it to spawn an entity.

//     /// The client can predict-spawn an entity, and it expects the server to also spawn the same entity when it receives
//     /// the information about the first entity.
//     /// Then the server replicates back its spawned entity to the client, and grabs authority over the entity.
//     /// All inputs that act on the entity after its spawned will be sent to the server.
//     PreSpawnedUserControlled {
//         /// the client entity that was pre-spawned and will be sent to the server
//         client_entity: Option<Entity>,
//         /// this is set by the server to know which client did the pre-prediction (in case the client is running
//         /// prediction for other client's entities as well)
//         client_id: Option<ClientId>,
//     },
//     /// The entity is created on both the client (in the predicted-timeline) and server side, preferably with the same system.
//     /// (for example a bullet that is shot by the player)
//     /// When the server replicates the bullet to the client, it finds the corresponding client prespawned entity and takes authority over it.
//     PreSpawnedPlayerObject {
//         /// Hash that will identify the entity
//         hash: u64,
//     },
// }

#[cfg(test)]
mod tests {
    use crate::client::prediction::predicted_history::{ComponentState, PredictionHistory};
    use bevy::prelude::Entity;
    use hashbrown::HashMap;

    use crate::client::prediction::resource::PredictionManager;

    use crate::prelude::*;
    use crate::tests::protocol::*;
    use crate::tests::stepper::{BevyStepper, Step};
    use crate::utils::ready_buffer::ItemWithReadyKey;

    #[test]
    fn test_compute_hash() {
        let mut stepper = BevyStepper::default();

        // check default compute hash, with multiple entities sharing the same tick
        stepper
            .client_app
            .world
            .spawn((Component1(1.0), PreSpawnedPlayerObject::default()));
        stepper
            .client_app
            .world
            .spawn((Component1(1.0), PreSpawnedPlayerObject::default()));
        stepper.frame_step();

        let current_tick = stepper.client_app.world.resource::<TickManager>().tick();
        let prediction_manager = stepper.client_app.world.resource::<PredictionManager>();
        let expected_hash: u64 = 6598339966483644418;
        assert_eq!(
            prediction_manager.prespawn_hash_to_entities,
            HashMap::from_iter(vec![(
                expected_hash,
                vec![Entity::from_raw(0), Entity::from_raw(1)]
            )])
        );
        assert_eq!(
            prediction_manager.prespawn_tick_to_hash.heap.peek(),
            Some(&ItemWithReadyKey {
                key: current_tick,
                item: expected_hash,
            })
        );

        // check that a PredictionHistory got added to the entity
        assert_eq!(
            stepper
                .client_app
                .world
                .entity(Entity::from_raw(0))
                .get::<PredictionHistory<Component1>>()
                .unwrap()
                .buffer
                .heap
                .peek(),
            Some(&ItemWithReadyKey {
                key: current_tick,
                item: ComponentState::Updated(Component1(1.0)),
            })
        );
    }
}
