#!/usr/bin/env python3
"""Regenerates tests/data/geo-test.mmdb and geo-test-swapped.mmdb, tiny GeoIP database in the City layout
that DB-IP and MaxMind ship (only `location.latitude` and `location.longitude`).

    pip install mmdb-writer netaddr
    python3 tests/data/make-geo-fixture.py

The addresses are documentation/private ranges, so the file says nothing about any
real network. It is committed; this script is here so the file can be reproduced.
"""
from netaddr import IPSet
from mmdb_writer import MMDBWriter

HK = {"location": {"latitude": 22.3, "longitude": 114.2}}
LONDON = {"location": {"latitude": 51.5, "longitude": -0.1}}
NEW_YORK = {"location": {"latitude": 40.7, "longitude": -74.0}}

def build(path, ranges):
    w = MMDBWriter(ip_version=6, database_type="GeoIP2-City", languages=["en"],
                   description={"en": "hbbs test fixture"}, ipv4_compatible=True)
    for net, place in ranges:
        w.insert_network(IPSet([net]), place)
    w.to_db_file(path)


# geo-test.mmdb: 10.40/16 is Hong Kong, 10.41/16 London, 10.42/16 New York.
build("tests/data/geo-test.mmdb", [
    ("10.40.0.0/16", HK), ("10.41.0.0/16", LONDON), ("10.42.0.0/16", NEW_YORK),
    ("2001:db8:41::/48", LONDON),
])
# geo-test-swapped.mmdb: what a later month's release might say after a range moved:
# 10.40/16 is now London and 10.41/16 Hong Kong. Used to test that an update lands.
build("tests/data/geo-test-swapped.mmdb", [
    ("10.40.0.0/16", LONDON), ("10.41.0.0/16", HK), ("10.42.0.0/16", NEW_YORK),
    ("2001:db8:41::/48", HK),
])
