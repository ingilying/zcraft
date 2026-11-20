// Local offline wire check, run by the opt-in client_compat test.
const assert = require('node:assert/strict')
const mc = require('minecraft-protocol')
const port = Number(process.env.ZCRAFT_TEST_PORT)
assert(Number.isInteger(port) && port > 0)

const clients = []
let finished = false
const timer = setTimeout(() => finish(new Error('multiplayer check timed out')), 15000)
function finish (error) {
  if (finished) return
  finished = true
  clearTimeout(timer)
  if (error) {
    console.error(error)
    process.exitCode = 1
  }
  for (const client of clients) {
    if (!client.ended) client.end()
  }
}

function connect (username) {
  const client = mc.createClient({
    host: '127.0.0.1', port, username, auth: 'offline', version: '1.21.1',
    profilesFolder: false, disableChatSigning: true
  })
  clients.push(client)
  client.on('error', finish)
  const packets = []
  const waiters = []
  const loaded = new Set()
  const entities = new Map()
  const tab = new Set()
  const chunk = (x, z) => `${x},${z}`
  const location = p => chunk(Math.floor(p.x / 16), Math.floor(p.z / 16))
  let ownId
  client.on('packet', (packet, meta) => {
    try {
      if (meta.state !== 'play') return
      if (meta.name === 'login') ownId = packet.entityId
      if (meta.name === 'position') client.write('teleport_confirm', { teleportId: packet.teleportId })
      if (meta.name === 'chunk_batch_finished') client.write('chunk_batch_received', { chunksPerTick: 8 })
      if (meta.name === 'map_chunk') loaded.add(chunk(packet.x, packet.z))
      if (meta.name === 'unload_chunk') {
        const id = chunk(packet.chunkX, packet.chunkZ)
        assert(![...entities.values()].includes(id), 'despawn must precede chunk unload')
        loaded.delete(id)
      }
      if (meta.name === 'player_info') {
        for (const entry of packet.data) {
          assert(!tab.has(entry.uuid), 'duplicate Tab entry')
          tab.add(entry.uuid)
        }
      }
      if (meta.name === 'player_remove') {
        for (const uuid of packet.players) assert(tab.delete(uuid), 'unknown Tab entry')
      }
      if (meta.name === 'spawn_entity') {
        assert.notEqual(packet.entityId, ownId)
        assert(tab.has(packet.objectUUID), 'Tab entry must precede spawn')
        assert(loaded.has(location(packet)), 'chunk must precede spawn')
        assert(!entities.has(packet.entityId), 'duplicate spawn')
        entities.set(packet.entityId, location(packet))
      }
      if (meta.name === 'entity_destroy') {
        for (const id of packet.entityIds) assert(entities.delete(id), 'unknown entity despawn')
      }
      if (['entity_teleport', 'entity_look', 'entity_head_rotation'].includes(meta.name)) {
        assert(entities.has(packet.entityId), 'update for an invisible entity')
        if (meta.name === 'entity_teleport') {
          assert(loaded.has(location(packet)), 'teleport into an unloaded chunk')
          entities.set(packet.entityId, location(packet))
        }
      }
      if (meta.name === 'kick_disconnect') throw new Error(JSON.stringify(packet))
      const idx = waiters.findIndex(w => w.name === meta.name && w.filter(packet))
      if (idx >= 0) waiters.splice(idx, 1)[0].resolve(packet)
      else packets.push([meta.name, packet])
    } catch (error) { finish(error) }
  })
  function take (name, filter = () => true) {
    const idx = packets.findIndex(([kind, packet]) => kind === name && filter(packet))
    if (idx >= 0) return Promise.resolve(packets.splice(idx, 1)[0][1])
    return new Promise(resolve => waiters.push({ name, filter, resolve }))
  }
  async function spawn (expectedName, expectedId, x) {
    const list = await take('player_info', p => p.data.some(e => e.player?.name === expectedName))
    const entry = list.data.find(e => e.player?.name === expectedName)
    assert.equal(entry.gamemode, 3)
    assert.equal(entry.listed, 1)
    const entity = await take('spawn_entity')
    assert.equal(entity.objectUUID, entry.uuid)
    assert.equal(entity.entityId, expectedId)
    assert.equal(entity.type, 128)
    assert.equal(entity.x, x)
    return entity.objectUUID
  }
  return { client, take, spawn, tab }
}

async function check () {
  const alice = connect('WireAlice')
  const oldId = (await alice.take('login')).entityId
  await alice.take('position')
  const bob = connect('WireBob')
  const bobId = (await bob.take('login')).entityId
  const [aliceUuid, bobUuid] = await Promise.all([bob.spawn('WireAlice', oldId, 8.5), alice.spawn('WireBob', bobId, 8.5)])
  alice.client.write('position_look', { x: 12, y: 65, z: 8.5, yaw: 90, pitch: -45, onGround: false })
  const move = await bob.take('entity_teleport')
  assert.deepEqual([move.entityId, move.x, move.y, move.pitch, move.yaw], [oldId, 12, 65, -32, 64])
  assert.deepEqual(await bob.take('entity_head_rotation'), { entityId: oldId, headYaw: 64 })
  alice.client.write('look', { yaw: 180, pitch: 0, onGround: true })
  assert.deepEqual(await bob.take('entity_look'), { entityId: oldId, yaw: -128, pitch: 0, onGround: true })
  assert.deepEqual(await bob.take('entity_head_rotation'), { entityId: oldId, headYaw: -128 })
  alice.client.write('position', { x: 160, y: 65, z: 8.5, onGround: false })
  assert.deepEqual((await bob.take('entity_destroy')).entityIds, [oldId])
  assert.deepEqual((await alice.take('entity_destroy')).entityIds, [bobId])
  await alice.take('unload_chunk', p => p.chunkX === 0 && p.chunkZ === 0)
  await alice.take('map_chunk', p => p.x === 10 && p.z === 0)
  assert(bob.tab.has(aliceUuid) && alice.tab.has(bobUuid), 'range exit must preserve Tab entries')
  alice.client.write('look', { yaw: 270, pitch: 0, onGround: false })
  alice.client.write('position', { x: 12, y: 65, z: 8.5, onGround: false })
  const [visibleAlice, visibleBob] = await Promise.all([bob.take('spawn_entity'), alice.take('spawn_entity')])
  assert.deepEqual([visibleAlice.entityId, visibleAlice.x, visibleAlice.yaw], [oldId, 12, -64])
  assert.deepEqual([visibleBob.entityId, visibleBob.x], [bobId, 8.5])
  assert(bob.tab.has(aliceUuid) && alice.tab.has(bobUuid))
  alice.client.end()
  assert.deepEqual((await bob.take('entity_destroy')).entityIds, [oldId])
  assert.deepEqual((await bob.take('player_remove')).players, [aliceUuid])
  const reconnect = connect('WireAlice')
  const newId = (await reconnect.take('login')).entityId
  assert.notEqual(oldId, newId)
  assert.equal(await bob.spawn('WireAlice', newId, 8.5), aliceUuid)
  await reconnect.spawn('WireBob', bobId, 8.5)
  console.log('PASS: independent 1.21.1 clients decoded mutual spawn, movement, head rotation, range despawn/reentry, stable Tab, and reconnect')
  finish()
}
check().catch(finish)
