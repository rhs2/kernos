-- Halcyon Provisions ledger (fictional). Schema for the gateway's sqlite
-- connector named "ledger" in gateway.json. The acceptance suite creates a
-- fresh database from this file before starting the gateway.

create table vendors (
  id    integer primary key,
  name  text not null,
  terms text
);

create table ledger_entries (
  id          integer primary key,
  invoice_id  text not null,
  vendor      text,
  account     text,
  amount      real,
  posted_at   text,
  voided_at   text,
  void_reason text
);

create index ledger_entries_invoice_id on ledger_entries (invoice_id);

insert into vendors (name, terms) values
  ('Northwind Dairy', 'net 30'),
  ('Harbor Greens', 'net 14'),
  ('Millstone Bakery', 'net 30');
