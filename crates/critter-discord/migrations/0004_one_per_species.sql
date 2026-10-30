-- One subscription per species per channel. Cadence becomes a setting that
-- `/critters configure` can change. Where a channel had both, keep daily.
delete from webhooks hourly using webhooks daily
where hourly.channel_id = daily.channel_id
  and hourly.species = daily.species
  and hourly.cadence = 'hourly'
  and daily.cadence = 'daily';
alter table webhooks
    drop constraint webhooks_channel_id_species_cadence_key,
    add unique (channel_id, species);
