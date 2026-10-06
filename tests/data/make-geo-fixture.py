#!/usr/bin/env python3
"""Regenerates tests/data/geo-test.mmdb, a tiny GeoIP database in the City layout
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

w = MMDBWriter(ip_version=6, database_type="GeoIP2-City", languages=["en"],
               description={"en": "hbbs test fixture"}, ipv4_compatible=True)
w.insert_network(IPSet(["10.40.0.0/16"]), HK)        # near the Hong Kong relay
w.insert_network(IPSet(["10.41.0.0/16"]), LONDON)    # near the London relay
w.insert_network(IPSet(["10.42.0.0/16"]), NEW_YORK)  # near the New York relay
w.insert_network(IPSet(["2001:db8:41::/48"]), LONDON)
w.to_db_file("tests/data/geo-test.mmdb")
