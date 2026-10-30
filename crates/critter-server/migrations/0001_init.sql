create table images (
    id          bigserial primary key,
    species     text not null,              -- 'wolf' | 'coyote' | 'fox'
    source      text not null,              -- 'commons' | 'inaturalist'
    source_id   text not null,
    image_url   text not null,
    source_url  text not null,
    author      text,
    author_url  text,
    license     text not null,              -- normalised: 'CC0-1.0', 'PDM', 'CC-BY-SA-4.0', ...
    license_url text,
    width       int,
    height      int,
    embedding   real[],                     -- null until embedded
    score       real,                       -- null until a model exists
    created_at  timestamptz not null default now(),
    unique (source, source_id)
);

create index images_species_score on images (species, score desc);

create table slots (
    species   text not null,
    kind      text not null,                -- 'hour' | 'day'
    starts_at timestamptz not null,
    image_id  bigint not null references images,
    primary key (species, kind, starts_at)
);

create table pairs (
    id         bigserial primary key,
    winner_id  bigint not null references images on delete cascade,
    loser_id   bigint not null references images on delete cascade,
    created_at timestamptz not null default now()
);

create table model (
    id         int primary key default 1 check (id = 1),
    weights    real[] not null,
    trained_at timestamptz not null default now()
);
