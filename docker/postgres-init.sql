-- Runs once when the compose Postgres volume is first created. The default
-- database (critter_server) comes from POSTGRES_DB. Each pusher gets its own.
create database critter_discord;
