-- One row per retrain, so accuracy can be compared across model versions.
create table training_runs (
    id                bigserial primary key,
    trained_at        timestamptz not null default now(),
    pairs             int not null,     -- training pairs used
    training_agreed   int not null,     -- training pairs the new model gets right
    validation_total  int not null,     -- validation pairs at the time
    validation_agreed int not null      -- validation pairs the new model gets right
);
