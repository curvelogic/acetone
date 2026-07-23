#!/bin/sh
# Build the asset-registry example graph used throughout the acetone manual.
# Run it in an empty directory with `acetone` on your PATH.
set -e

acetone init

# --- Schema: natural keys first, then constraints and an index ---------------

acetone declare-label Team --key name
acetone declare-label Host --key name
acetone declare-label Service --key name --require tier
acetone declare-rel-type OWNS
acetone declare-rel-type RUNS_ON
acetone declare-rel-type DEPENDS_ON
acetone declare-index host_by_region --label Host --property region

# --- Nodes -------------------------------------------------------------------

acetone query 'CREATE (:Team {name: "platform", oncall: "#platform-oncall"}),
                      (:Team {name: "payments", oncall: "#payments-oncall"}),
                      (:Team {name: "web",      oncall: "#web-oncall"})'

acetone query 'CREATE (:Host {name: "db1",   region: "eu-west",    os: "linux"}),
                      (:Host {name: "db2",   region: "eu-central", os: "linux"}),
                      (:Host {name: "app1",  region: "eu-west",    os: "linux"}),
                      (:Host {name: "app2",  region: "eu-central", os: "linux"}),
                      (:Host {name: "edge1", region: "eu-west",    os: "freebsd"})'

acetone query 'CREATE (:Service {name: "postgres",   tier: "data", version: "16.3"}),
                      (:Service {name: "identity",   tier: "core", version: "2.4.1"}),
                      (:Service {name: "billing",    tier: "core", version: "7.0.2"}),
                      (:Service {name: "storefront", tier: "edge", version: "2026.28"})'

# --- Ownership: every service belongs to exactly one team --------------------

acetone query 'MATCH (t:Team {name: "platform"}),
                     (pg:Service {name: "postgres"}), (id:Service {name: "identity"})
               CREATE (t)-[:OWNS]->(pg), (t)-[:OWNS]->(id)'
acetone query 'MATCH (t:Team {name: "payments"}), (s:Service {name: "billing"})
               CREATE (t)-[:OWNS]->(s)'
acetone query 'MATCH (t:Team {name: "web"}), (s:Service {name: "storefront"})
               CREATE (t)-[:OWNS]->(s)'

# --- Placement: where each service runs --------------------------------------

acetone query 'MATCH (s:Service {name: "postgres"}),
                     (a:Host {name: "db1"}), (b:Host {name: "db2"})
               CREATE (s)-[:RUNS_ON]->(a), (s)-[:RUNS_ON]->(b)'
acetone query 'MATCH (s:Service {name: "identity"}), (h:Host {name: "app1"})
               CREATE (s)-[:RUNS_ON]->(h)'
acetone query 'MATCH (s:Service {name: "billing"}),
                     (a:Host {name: "app1"}), (b:Host {name: "app2"})
               CREATE (s)-[:RUNS_ON]->(a), (s)-[:RUNS_ON]->(b)'
acetone query 'MATCH (s:Service {name: "storefront"}), (h:Host {name: "edge1"})
               CREATE (s)-[:RUNS_ON]->(h)'

# --- Dependencies: the service call graph ------------------------------------

acetone query 'MATCH (s:Service {name: "identity"}), (d:Service {name: "postgres"})
               CREATE (s)-[:DEPENDS_ON]->(d)'
acetone query 'MATCH (s:Service {name: "billing"}),
                     (pg:Service {name: "postgres"}), (id:Service {name: "identity"})
               CREATE (s)-[:DEPENDS_ON]->(pg), (s)-[:DEPENDS_ON]->(id)'
acetone query 'MATCH (s:Service {name: "storefront"}),
                     (b:Service {name: "billing"}), (id:Service {name: "identity"})
               CREATE (s)-[:DEPENDS_ON]->(b), (s)-[:DEPENDS_ON]->(id)'

# --- One commit for the whole seed -------------------------------------------

acetone commit -m "asset registry: initial inventory"
