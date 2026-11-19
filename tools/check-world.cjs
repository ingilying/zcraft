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
let registries = 0
let heartbeats = 0
let positioned = false
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
    }
    if (meta.name === 'map_chunk') {
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
      assert.equal(packet.batchSize, 25)
      assert.equal(chunks, 25)
      client.write('chunk_batch_received', { chunksPerTick: 20 })
    }
    if (meta.name === 'keep_alive') heartbeats++
  } catch (error) { fail(error) }
})
finishTimer = setTimeout(() => {
  if (completed) return
  try {
    assert(joined && positioned)
    assert.equal(registries, 11)
    assert.equal(chunks, 25)
    assert(heartbeats >= 3)
    assert(Date.now() - started > 30000)
    completed = true
    clearTimeout(deadline)
    console.log(`PASS: ${version}, ${registries} registries, ${chunks} decoded terrain/light chunks, ${heartbeats} heartbeats, Play alive beyond 30s`)
    client.end()
  } catch (error) { fail(error) }
}, 34000)
