# Administrative logical inventory

`GET /v1/inventory` is an administrator-only, read-only inventory of the
currently opened persisted PreppyDB view. It is computed directly from the
engine state and does not trust migration receipts or importer progress. It
returns a content SHA-256 root, sequence and checkpoint generation, all
collection names, physical document counts (including expired documents not
yet swept), serialized byte totals, collection metadata hashes, and each
collection's ordered document hash root.

`GET /v1/inventory/{collection}/documents?sequence=N&generation=G&limit=L&after=K`
returns bounded, key-ordered identities, canonical serialized byte lengths,
and SHA-256 hashes. Pages are accepted only while both the WAL sequence and
checkpoint generation still match the manifest. Any intervening write or
checkpoint returns a conflict and requires a fresh inventory. The page returns
no document values. Both endpoints require the `admin` role because collection
names and keys can themselves be sensitive operational metadata.

All 64-bit counts, byte lengths, WAL sequences and generations are decimal
strings in this endpoint so clients such as Lua 5.1 do not lose precision above
2^53. The client must preserve and return those strings verbatim.

The digest format is domain-separated SHA-256 over deterministic PreppyDB JSON
canonical bytes. A document digest binds its collection, key, complete stored
document envelope (value and version/expiry metadata), and encoded length. The
collection root binds sorted document identities and digests plus collection
metadata. The database root binds the ordered collection inventory and WAL
sequence. Checkpoint generation participates in page-staleness checks and the
snapshot ID, but not the content root, so the root remains independently
comparable after a clean restart/checkpoint when logical data is unchanged.

This endpoint is a point-in-time logical view held under the engine's read lock
while the summary is calculated. It does not create a durable snapshot lease or
export payloads. A consumer that needs to reconcile pages must stop writers or
restart the inventory if the sequence/generation changes. It is not a backup,
restore, migration, or cutover API. It does not provide replication or
physical-power-loss qualification.

The existing virtual-currency policy remains in force. These generic inventory
endpoints do not authorize importing Molly wallet, balance, receipt, or ledger
data, including if those fields are nested in another document.
