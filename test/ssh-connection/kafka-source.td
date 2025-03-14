# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

# Test creating a Kafka source using SSH, with and without the CSR

$ kafka-create-topic topic=thetopic

$ kafka-ingest topic=thetopic format=bytes
one

# Create a dedicated cluster for the sources, so we can easily restart the
# sources by dropping and recreating the cluster's replica.
#
# We also use a large number of worker threads to protect against past behavior
# in which we opened one SSH tunnel per worker thread per broker, which
# tripped the default `MaxStartups 10` sshd configuration.
> DROP CLUSTER IF EXISTS sc;
> CREATE CLUSTER sc REPLICAS (r1 (SIZE '32'))

# Test the various types of tunnels

> CREATE CONNECTION kafka_conn_using
  TO KAFKA (BROKER '${testdrive.kafka-addr}' USING SSH TUNNEL thancred, SECURITY PROTOCOL PLAINTEXT);

> CREATE CONNECTION kafka_conn_dynamic
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT, SSH TUNNEL thancred);

> CREATE SOURCE fixed_text IN CLUSTER sc
  FROM KAFKA CONNECTION kafka_conn_using (TOPIC 'testdrive-thetopic-${testdrive.seed}')

> CREATE TABLE fixed_text_tbl FROM SOURCE fixed_text (REFERENCE "testdrive-thetopic-${testdrive.seed}")
  FORMAT TEXT
  ENVELOPE NONE

> CREATE SOURCE dynamic_text  IN CLUSTER sc
  FROM KAFKA CONNECTION kafka_conn_dynamic (TOPIC 'testdrive-thetopic-${testdrive.seed}')

> CREATE TABLE dynamic_text_tbl FROM SOURCE dynamic_text (REFERENCE "testdrive-thetopic-${testdrive.seed}")
  FORMAT TEXT
  ENVELOPE NONE

> SELECT * FROM fixed_text_tbl
text
----
one

$ kafka-ingest topic=thetopic format=bytes
two

# Ensure both types of tunnels work
> SELECT * FROM fixed_text_tbl
text
----
one
two

> SELECT * FROM dynamic_text_tbl
text
----
one
two

> CREATE CONNECTION csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}',
    SSH TUNNEL thancred
  );

$ set schema={
        "type" : "record",
        "name" : "test",
        "fields" : [
            {"name":"f1", "type":"string"},
            {"name":"f2", "type":"long"}
        ]
    }

$ kafka-create-topic topic=avroavro

$ kafka-ingest format=avro topic=avroavro schema=${schema}
{"f1": "fish", "f2": 1000}

> CREATE SOURCE fixed_plus_csr
  IN CLUSTER sc
  FROM KAFKA CONNECTION kafka_conn_using (TOPIC 'testdrive-avroavro-${testdrive.seed}')

> CREATE TABLE fixed_plus_csr_tbl FROM SOURCE fixed_plus_csr (REFERENCE "testdrive-avroavro-${testdrive.seed}")
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE NONE

> SELECT * FROM fixed_plus_csr_tbl
f1    f2
----------
fish  1000

# Test csr sources for dynamic connections as well.
> CREATE SOURCE dynamic_plus_csr
  IN CLUSTER sc
  FROM KAFKA CONNECTION kafka_conn_dynamic (TOPIC 'testdrive-avroavro-${testdrive.seed}')

> CREATE TABLE dynamic_plus_csr_tbl FROM SOURCE dynamic_plus_csr (REFERENCE "testdrive-avroavro-${testdrive.seed}")
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE NONE

> SELECT * FROM dynamic_plus_csr_tbl
f1    f2
----------
fish  1000

# ensure they all were marked as running correctly
> SELECT status FROM mz_internal.mz_source_statuses st
  JOIN mz_sources s ON st.id = s.id
  WHERE s.name in ('fixed_text', 'dynamic_text', 'fixed_plus_csr', 'dynamic_plus_csr')
running
running
running
running
