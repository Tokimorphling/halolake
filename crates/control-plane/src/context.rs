certain_map::certain_map! {
    #[empty(_ControlContextEmpty)]
    #[full(_FullControlContext)]
    #[style = "unfilled"]
    #[derive(Clone)]
    pub struct ControlContext {
        request_id: ControlRequestId,
        actor: ControlActor,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlRequestId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlActor {
    System,
    Admin { user_id: String },
}
