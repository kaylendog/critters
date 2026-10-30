-- Subscriptions now come from slash commands, so each row belongs to a
-- channel. Rows from the old admin API have no channel and are dropped.
delete from webhooks;
alter table webhooks
    add column guild_id   bigint not null,
    add column channel_id bigint not null,
    add unique (channel_id, species, cadence);
