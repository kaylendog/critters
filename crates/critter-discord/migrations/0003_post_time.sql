-- When a daily subscription posts, as a local time in an IANA time zone.
-- Hourly subscriptions ignore both.
alter table webhooks
    add column post_at  time not null default '00:00',
    add column timezone text not null default 'UTC';
