# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-arg-default single-replica-cluster=quickstart

# This test exercises UUIDs at the boundary (e.g., by sending them
# through pgwire). Operations on UUIDs are more thoroughly tested in
# uuid.slt.



$ set schema={
    "name": "row",
    "type": "record",
    "fields": [
      {
        "name": "u",
        "type": {
          "type": "string",
          "logicalType": "uuid"
        }
      }
    ]
  }

$ kafka-create-topic topic=data

$ kafka-ingest format=avro topic=data schema=${schema}
{"u": "16fd95b0-65b7-4249-9b66-1547cd95923d"}
{"u": "b141698b-fb7f-492d-bc8a-0d159641c7a3"}

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE data
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-data-${testdrive.seed}')

> CREATE TABLE data_tbl FROM SOURCE data (REFERENCE "testdrive-data-${testdrive.seed}")
  FORMAT AVRO USING SCHEMA '${schema}'

> SHOW COLUMNS FROM data_tbl
name   nullable  type  comment
------------------------------
u      false     uuid  ""

> SELECT * FROM data_tbl
"16fd95b0-65b7-4249-9b66-1547cd95923d"
"b141698b-fb7f-492d-bc8a-0d159641c7a3"

> SELECT '85907cb9-ac9b-4e35-84b8-60dc69368aca'::uuid
"85907cb9-ac9b-4e35-84b8-60dc69368aca"

> SELECT '85907cb9-ac9b-4e35-84b8-60dc69368aca'::uuid::text
"85907cb9-ac9b-4e35-84b8-60dc69368aca"

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

> CREATE SINK uuid_sink_${testdrive.seed}
  IN CLUSTER ${arg.single-replica-cluster}
  FROM data_tbl
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-sink-data-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

$ kafka-verify-data format=avro sort-messages=true sink=materialize.public.uuid_sink_${testdrive.seed}
{"before": null, "after": {"row":{"u": "16fd95b0-65b7-4249-9b66-1547cd95923d"}}}
{"before": null, "after": {"row":{"u": "b141698b-fb7f-492d-bc8a-0d159641c7a3"}}}
