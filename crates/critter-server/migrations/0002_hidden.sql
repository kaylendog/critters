-- Images an admin never wants served, whatever the ranker thinks of them.
alter table images add column hidden boolean not null default false;
