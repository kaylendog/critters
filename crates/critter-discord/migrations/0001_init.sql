create table webhooks (
    id              uuid primary key,
    url             text not null,
    species         text not null,
    cadence         text not null,          -- 'hourly' | 'daily'
    last_fired_slot timestamptz,
    created_at      timestamptz not null default now()
);
