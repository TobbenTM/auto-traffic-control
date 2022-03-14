use std::sync::Arc;

use tonic::{Request, Response, Status};

use atc::v1::update_flight_plan_response::Payload;
use atc::v1::{
    GetAirplaneRequest, GetAirplaneResponse, UpdateFlightPlanError, UpdateFlightPlanRequest,
    UpdateFlightPlanResponse, UpdateFlightPlanSuccess,
};

use crate::command::CommandSender;
use crate::components::{AirplaneId, FlightPlan};
use crate::store::Store;
use crate::Command;

#[derive(Clone, Debug)]
pub struct AirplaneService {
    command_bus: CommandSender,
    store: Arc<Store>,
}

impl AirplaneService {
    pub fn new(command_bus: CommandSender, store: Arc<Store>) -> Self {
        Self { command_bus, store }
    }
}

#[tonic::async_trait]
impl atc::v1::airplane_service_server::AirplaneService for AirplaneService {
    async fn get_airplane(
        &self,
        request: Request<GetAirplaneRequest>,
    ) -> Result<Response<GetAirplaneResponse>, Status> {
        let id = request.into_inner().id;

        if let Some(airplane) = self.store.get(&id) {
            Ok(Response::new(GetAirplaneResponse {
                airplane: Some(airplane.clone()),
            }))
        } else {
            Err(Status::not_found(&format!(
                "No airplane with id {id} was found"
            )))
        }
    }

    async fn update_flight_plan(
        &self,
        request: Request<UpdateFlightPlanRequest>,
    ) -> Result<Response<UpdateFlightPlanResponse>, Status> {
        let request = request.into_inner();
        let id = request.id;

        let airplane = match self.store.get(&id) {
            Some(airplane) => airplane,
            None => {
                return Err(Status::not_found(&format!(
                    "No airplane with id {id} was found"
                )));
            }
        };

        let previous_flight_plan = (&airplane.flight_plan).into();
        let new_flight_plan: FlightPlan = (&request.flight_plan).into();

        if let Err(errors) = new_flight_plan.validate(&previous_flight_plan) {
            let errors = errors.iter().map(|error| (*error).into()).collect();

            return Ok(Response::new(UpdateFlightPlanResponse {
                payload: Some(Payload::Error(UpdateFlightPlanError { errors })),
            }));
        };

        if self
            .command_bus
            .send(Command::UpdateFlightPlan(
                AirplaneId::new(id),
                new_flight_plan,
            ))
            .is_err()
        {
            return Err(Status::internal("failed to queue command"));
        }

        Ok(Response::new(UpdateFlightPlanResponse {
            payload: Some(Payload::Success(UpdateFlightPlanSuccess {})),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::broadcast::channel;
    use tonic::{Code, Request};

    use atc::v1::airplane_service_server::AirplaneService as ServiceTrait;
    use atc::v1::update_flight_plan_error::ValidationError;
    use atc::v1::update_flight_plan_response::Payload;
    use atc::v1::{Airplane, GetAirplaneRequest, UpdateFlightPlanRequest};

    use crate::api::airplane::AirplaneService;
    use crate::api::IntoApi;
    use crate::command::CommandReceiver;
    use crate::components::{AirplaneId, FlightPlan, Location};
    use crate::map::{Tile, MAP_HEIGHT_RANGE, MAP_WIDTH_RANGE};
    use crate::{Command, Store};

    fn setup() -> (CommandReceiver, Arc<Store>, AirplaneService) {
        let (command_sender, command_receiver) = channel::<Command>(1024);
        let store = Arc::new(Store::new());
        let service = AirplaneService::new(command_sender, store.clone());

        (command_receiver, store, service)
    }

    fn init_airplane(id: &str, store: &Arc<Store>) -> (AirplaneId, Location, FlightPlan) {
        let id = AirplaneId::new(id.into());
        let location = Location::new(0, 0);
        let flight_plan = FlightPlan::new(vec![Tile::new(0, 0)]);

        let airplane = Airplane {
            id: id.clone().into_api(),
            location: Some(location.into_api()),
            flight_plan: flight_plan.clone().into_api(),
        };

        store.insert("AT-4321".into(), airplane);

        (id, location, flight_plan)
    }

    #[tokio::test]
    async fn get_airplane_with_wrong_id() {
        let (_command_bus, _store, service) = setup();

        let request = Request::new(GetAirplaneRequest {
            id: "AT-4321".into(),
        });
        let status = service.get_airplane(request).await.unwrap_err();

        assert_eq!(status.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn get_airplane_for_existing_plane() {
        let (_command_bus, store, service) = setup();
        let (_id, _location, _flight_plan) = init_airplane("AT-4321", &store);

        let request = Request::new(GetAirplaneRequest {
            id: "AT-4321".into(),
        });
        let response = service.get_airplane(request).await.unwrap();

        let payload = response.into_inner();
        let airplane = payload.airplane.unwrap();

        assert_eq!("AT-4321", &airplane.id);
    }

    #[tokio::test]
    async fn update_flight_plan_with_wrong_id() {
        let (_command_bus, _store, service) = setup();

        let request = Request::new(UpdateFlightPlanRequest {
            id: "AT-4321".into(),
            flight_plan: vec![Tile::new(0, 0).into_api()],
        });
        let status = service.update_flight_plan(request).await.unwrap_err();

        assert_eq!(status.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn update_flight_plan_with_invalid_plan() {
        let (mut command_bus, store, service) = setup();
        let (_id, _location, _flight_plan) = init_airplane("AT-4321", &store);

        let request = Request::new(UpdateFlightPlanRequest {
            id: "AT-4321".into(),
            flight_plan: vec![
                Tile::new(1, 0).into_api(),
                Tile::new(3, 0).into_api(),
                Tile::new(1, 0).into_api(),
                Tile::new(MAP_WIDTH_RANGE.start() - 1, MAP_HEIGHT_RANGE.start() - 1).into_api(),
            ],
        });
        let response = service.update_flight_plan(request).await.unwrap();

        let actual_errors = match response.into_inner().payload.unwrap() {
            Payload::Error(error) => error.errors,
            _ => panic!("unexpected payload"),
        };
        let expected_errors: Vec<i32> = vec![
            ValidationError::NodeOutOfBounds.into(),
            ValidationError::NotInLogicalOrder.into(),
            ValidationError::InvalidFirstNode.into(),
            ValidationError::HasSharpTurns.into(),
        ];

        assert_eq!(expected_errors, actual_errors);
        assert!(command_bus.try_recv().is_err());
    }

    #[tokio::test]
    async fn update_flight_plan_fails_to_queue_command() {
        let (command_bus, store, service) = setup();
        std::mem::drop(command_bus);

        let id = AirplaneId::new("AT-4321".into());
        let location = Location::new(0, 0);
        let flight_plan = FlightPlan::new(vec![Tile::new(0, 0)]);

        let airplane = Airplane {
            id: id.into_api(),
            location: Some(location.into_api()),
            flight_plan: flight_plan.into_api(),
        };

        store.insert("AT-4321".into(), airplane);

        let request = Request::new(UpdateFlightPlanRequest {
            id: "AT-4321".into(),
            flight_plan: vec![Tile::new(0, 0).into_api()],
        });
        let status = service.update_flight_plan(request).await.unwrap_err();

        assert_eq!(status.code(), Code::Internal);
    }

    #[tokio::test]
    async fn update_flight_plan_with_valid_plan() {
        let (mut command_bus, store, service) = setup();

        let id = AirplaneId::new("AT-4321".into());
        let location = Location::new(0, 0);
        let flight_plan = FlightPlan::new(vec![Tile::new(0, 0)]);

        let airplane = Airplane {
            id: id.into_api(),
            location: Some(location.into_api()),
            flight_plan: flight_plan.into_api(),
        };

        store.insert("AT-4321".into(), airplane);

        let new_flight_plan = FlightPlan::new(vec![Tile::new(0, 0), Tile::new(1, 0)]);

        let request = Request::new(UpdateFlightPlanRequest {
            id: "AT-4321".into(),
            flight_plan: new_flight_plan.clone().into_api(),
        });
        let response = service.update_flight_plan(request).await.unwrap();

        if let Payload::Error(_) = response.into_inner().payload.unwrap() {
            panic!("unexpected payload");
        }

        let command = command_bus.try_recv().unwrap();
        let Command::UpdateFlightPlan(airplane_id, flight_plan) = command;

        assert_eq!("AT-4321", airplane_id.get());
        assert_eq!(new_flight_plan, flight_plan);
    }
}