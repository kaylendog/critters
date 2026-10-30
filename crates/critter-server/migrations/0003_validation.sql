-- Validation pairs measure the ranker on images it has not trained on. They
-- are excluded from training until promoted.
alter table pairs add column validation boolean not null default false;
