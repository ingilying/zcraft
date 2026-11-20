// Optional independent wire check. Dependencies: minecraft-protocol,
// prismarine-chunk (install outside the repository and expose via NODE_PATH).
// Invoked by tests/client_compat.rs; never reads accounts or auth caches.
const assert = require('node:assert/strict')
const mc = require('minecraft-protocol')
const Chunk = require('prismarine-chunk')('1.21.1')
const version = '1.21.1'
const port = Number(process.env.ZCRAFT_TEST_PORT)
assert(Number.isInteger(port) && port > 0)
const client = mc.createClient({
  host: '127.0.0.1', port, username: 'WireTest', auth: 'offline', version,
  profilesFolder: false, disableChatSigning: true,
  keepAlive: true, hideErrors: false
})
let chunks = 0
let batchCount = 0
let center = '0,0'
const loaded = new Set()
const route = [[0, 0], [1, 0], [10, -4], [-1, -1]]
let step = 0
let unloads = 0
function key (x, z) { return `${x},${z}` }
function expected (x, z) {
  const chunks = []
  for (let dx = -2; dx <= 2; dx++) {
    for (let dz = -2; dz <= 2; dz++) chunks.push(key(x + dx, z + dz))
  }
  return chunks.sort()
}
let registries = 0
let heartbeats = 0
let positioned = false
let spectatorAck = false
let joined = false
let completed = false
let finishTimer
const started = Date.now()
const deadline = setTimeout(() => fail(new Error('world check exceeded 45 seconds')), 45000)
function fail (error) {
  if (completed) return
  completed = true
  clearTimeout(deadline)
  clearTimeout(finishTimer)
  console.error(error)
  client.end()
  process.exitCode = 1
}
client.on('error', fail)
client.on('end', reason => {
  if (!completed) fail(new Error(`unexpected disconnect: ${reason}`))
})
client.on('disconnect', packet => fail(new Error(JSON.stringify(packet))))
client.on('kick_disconnect', packet => fail(new Error(JSON.stringify(packet))))
client.on('packet', (packet, meta) => {
  try {
    if (meta.name === 'registry_data') {
      registries++
      assert(packet.entries.every(entry => entry.value))
      if (packet.id === 'minecraft:worldgen/biome') assert.equal(packet.entries[0].key, 'minecraft:plains')
      if (packet.id === 'minecraft:dimension_type') assert.equal(packet.entries[0].key, 'minecraft:overworld')
    }
    if (meta.state !== 'play') return
    if (meta.name === 'login') {
      joined = true
      assert.equal(packet.worldState.gamemode, 'spectator')
      assert.equal(packet.worldState.dimension, 0)
      assert.equal(packet.viewDistance, 2)
    }
    if (meta.name === 'position') {
      positioned = true
      assert.equal(packet.x, 8.5)
      assert.equal(packet.y, 65)
      client.write('teleport_confirm', { teleportId: packet.teleportId })
      client.write('position', { x: 8.5, y: 65, z: 8.5, onGround: false })
      client.write('block_dig', { status: 0, location: { x: 8, y: 63, z: 8 }, face: 1, sequence: 1 })
    }
    if (meta.name === 'update_view_position') center = key(packet.chunkX, packet.chunkZ)
    if (meta.name === 'unload_chunk') {
      assert(loaded.delete(key(packet.chunkX, packet.chunkZ)), 'unload must refer to a loaded chunk')
      unloads++
    }
    if (meta.name === 'chunk_batch_start') batchCount = 0
    if (meta.name === 'map_chunk') {
      const id = key(packet.x, packet.z)
      assert(!loaded.has(id), 'duplicate chunk')
      loaded.add(id)
      batchCount++
      const chunk = new Chunk({ minY: -64, worldHeight: 384 })
      chunk.load(packet.chunkData)
      chunk.loadParsedLight(packet.skyLight, packet.blockLight,
        packet.skyLightMask, packet.blockLightMask,
        packet.emptySkyLightMask, packet.emptyBlockLightMask)
      const pos = y => ({ x: 0, y, z: 0 })
      assert.equal(chunk.getBlockStateId(pos(60)), 79)
      assert.equal(chunk.getBlockStateId(pos(61)), 10)
      assert.equal(chunk.getBlockStateId(pos(63)), 9)
      assert.equal(chunk.getBlockStateId(pos(64)), 0)
      assert.equal(chunk.getSkyLight(pos(65)), 15)
      chunks++
    }
    if (meta.name === 'chunk_batch_finished') {
      assert.equal(packet.batchSize, batchCount)
      assert(batchCount > 0 && batchCount <= 8)
      client.write('chunk_batch_received', { chunksPerTick: 20 })
      if (step < route.length && loaded.size === 25) {
        const [x, z] = route[step++]
        assert.equal(center, key(x, z))
        assert.deepEqual([...loaded].sort(), expected(x, z))
        if (step < route.length) {
          const [nextX, nextZ] = route[step]
          client.write('position', { x: nextX * 16 + 0.5, y: 65, z: nextZ * 16 + 0.5, onGround: false })
        }
      }
    }
    if (meta.name === 'acknowledge_player_digging') spectatorAck = packet.sequenceId === 1
    assert.notEqual(meta.name, 'block_change', 'spectator must not edit terrain')
    if (meta.name === 'keep_alive') heartbeats++
  } catch (error) { fail(error) }
})
finishTimer = setTimeout(() => {
  if (completed) return
  try {
    assert(joined && positioned && spectatorAck)
    assert.equal(registries, 11)
    assert.equal(step, route.length)
    assert.equal(chunks, 80)
    assert.equal(unloads, 55)
    assert(heartbeats >= 3)
    assert(Date.now() - started > 30000)
    completed = true
    clearTimeout(deadline)
    console.log(`PASS: ${version}, ${registries} registries, ${chunks} decoded terrain/light chunks, ${unloads} unloads, ${heartbeats} heartbeats, Play alive beyond 30s`)
    client.end()
  } catch (error) { fail(error) }
}, 34000)
