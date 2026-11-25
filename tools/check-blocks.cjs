// Independent Minecraft Java 1.21.1 creative-mode wire check; runs on loopback.
const assert = require('node:assert/strict')
const mc = require('minecraft-protocol')
const Chunk = require('prismarine-chunk')('1.21.1')
const md = require('minecraft-data')('1.21.1')
const port = Number(process.env.ZCRAFT_TEST_PORT)
assert(Number.isInteger(port) && port > 0)
const start = Date.now()
const peers = []
let failure

const same = (a, b) => a.x === b.x && a.y === b.y && a.z === b.z
function client (username) {
  const wire = mc.createClient({ host: '127.0.0.1', port, username, auth: 'offline', version: '1.21.1', profilesFolder: false, disableChatSigning: true })
  peers.push(wire)
  const events = []
  wire.on('error', err => { failure = err })
  wire.on('kick_disconnect', packet => { failure = new Error(JSON.stringify(packet)) })
  wire.on('end', reason => { failure ??= new Error(`${username}: disconnected ${reason}`) })
  wire.on('packet', (packet, meta) => {
    if (meta.state !== 'play') return
    events.push([meta.name, packet])
    if (meta.name === 'position') wire.write('teleport_confirm', { teleportId: packet.teleportId })
    if (meta.name === 'chunk_batch_finished') wire.write('chunk_batch_received', { chunksPerTick: 8 })
  })
  return {
    events,
    write: (name, packet) => wire.write(name, packet),
    async expect (name, predicate = () => true, since = 0) {
      for (;;) {
        if (failure) throw failure
        const event = events.slice(since).find(([n, p]) => n === name && predicate(p))
        if (event) return event[1]
        assert(Date.now() - start < 20_000, `${username}: timed out waiting for ${name}`)
        await new Promise(resolve => setTimeout(resolve, 10))
      }
    }
  }
}

const slot = (peer, slotId) => peer.write('held_item_slot', { slotId })
const move = (peer, x, z) => peer.write('position', { x, y: 65, z, onGround: false })
const place = (peer, location, sequence, hand = 0) => peer.write('block_place', {
  hand, location, direction: 1, cursorX: 0.5, cursorY: 1, cursorZ: 0.5, insideBlock: false, sequence
})
const dig = (peer, location, sequence) => peer.write('block_dig', { status: 0, location, face: 1, sequence })
const ack = (peer, sequenceId) => peer.expect('acknowledge_player_digging', p => p.sequenceId === sequenceId)
const emptyStack = { itemCount: 0 }
const stack = (itemId, itemCount = 64) => ({ itemCount, itemId, addedComponentCount: 0, removedComponentCount: 0, components: [], removeComponents: [] })
const click = (peer, stateId, location, cursorItem, changedItem) => peer.write('window_click', {
  windowId: 0, stateId, slot: location, mouseButton: 0, mode: 0,
  changedSlots: [{ location, item: changedItem }], cursorItem
})
const change = (peer, location, type, since) => peer.expect('block_change', p => same(p.location, location) && p.type === type, since)
const chunkWith = (packet) => {
  const chunk = new Chunk({ minY: -64, worldHeight: 384 })
  chunk.load(packet.chunkData)
  chunk.loadParsedLight(packet.skyLight, packet.blockLight, packet.skyLightMask, packet.blockLightMask,
    packet.emptySkyLightMask, packet.emptyBlockLightMask)
  return chunk
}

async function run () {
  const a = client('BuildA')
  const b = client('BuildB')
  for (const p of [a, b]) {
    assert.equal((await p.expect('login')).worldState.gamemode, 'creative')
    assert.equal((await p.expect('abilities')).flags, 0x0d)
    const inventory = await p.expect('window_items')
    assert.equal(inventory.items.length, 46)
    assert.deepEqual(inventory.items.slice(36, 45).map(item => item.itemId), [
      'stone', 'dirt', 'glass', 'wooden_pickaxe', 'stone_pickaxe', 'iron_pickaxe',
      'wooden_shovel', 'stone_shovel', 'iron_shovel'
    ].map(n => md.itemsByName[n].id))
    await p.expect('position')
  }
  const support = { x: 10, y: 63, z: 8 }
  const target = { ...support, y: 64 }
  const stone = md.blocksByName.stone.defaultState
  const glass = md.blocksByName.glass.defaultState
  const dirt = md.blocksByName.dirt.defaultState
  let cursorA = a.events.length
  let cursorB = b.events.length
  place(a, support, 1)
  await change(a, target, stone, cursorA)
  await ack(a, 1)
  assert(a.events.findIndex(([n, p], i) => i >= cursorA && n === 'block_change' && same(p.location, target)) < a.events.findIndex(([n, p]) => n === 'acknowledge_player_digging' && p.sequenceId === 1))
  await change(b, target, stone, cursorB)
  const light = await b.expect('update_light', p => p.chunkX === 0 && p.chunkZ === 0, cursorB)
  // This decoder's light helper reads groups as big-endian longs; the wire
  // sends raw nibble arrays. Read individual samples directly for mixed light.
  const mask = BigInt(light.skyLightMask[0][1] >>> 0)
  function skylight ({ x, y, z }) {
    const section = (y + 64 >> 4) + 1
    let arrayIndex = 0
    for (let i = 0; i < section; i++) if (mask & (1n << BigInt(i))) arrayIndex++
    if (!(mask & (1n << BigInt(section)))) return 0
    const sample = (y & 15) * 256 + z * 16 + x
    return (light.skyLight[arrayIndex][sample >> 1] >> (4 * (sample & 1))) & 15
  }
  assert.equal(skylight(target), 0)
  assert.equal(skylight({ ...target, y: 65 }), 15)
  assert.equal(skylight({ ...target, x: 11 }), 15)

  // A block item is not a tool. Wooden pickaxe meets stone's requirement.
  cursorA = a.events.length
  dig(a, target, 2)
  await change(a, target, stone, cursorA)
  await ack(a, 2)
  slot(a, 3)
  cursorB = b.events.length
  dig(a, target, 3)
  await ack(a, 3)
  await change(b, target, 0, cursorB)
  move(b, 160.5, -160.5)
  await b.expect('unload_chunk', p => p.chunkX === 0 && p.chunkZ === 0, cursorB)
  // Out-of-range item IDs are rejected; valid vanilla catalog items are accepted.
  cursorA = a.events.length
  a.write('set_creative_slot', { slot: 38, item: stack(9999) })
  const rejected = await a.expect('window_items', () => true, cursorA)
  assert.equal(rejected.items[38].itemId, md.itemsByName.glass.id)
  cursorA = a.events.length
  a.write('set_creative_slot', { slot: 38, item: stack(md.itemsByName.iron_ore.id) })
  await a.expect('window_items', p => p.items[38].itemId === md.itemsByName.iron_ore.id, cursorA)
  const ironOre = md.blocksByName.iron_ore.defaultState
  slot(a, 2)
  place(a, support, 4)
  await change(a, target, ironOre, cursorA)
  await ack(a, 4)
  // Correct kind but insufficient tier is denied; stone-tier pickaxe succeeds.
  slot(a, 3)
  cursorA = a.events.length
  dig(a, target, 5)
  await change(a, target, ironOre, cursorA)
  await ack(a, 5)
  slot(a, 4)
  dig(a, target, 6)
  await change(a, target, 0, cursorA)
  await ack(a, 6)
  cursorA = a.events.length
  a.write('set_creative_slot', { slot: 38, item: stack(md.itemsByName.glass.id) })
  const corrected = await a.expect('window_items', p => p.items[38].itemId === md.itemsByName.glass.id, cursorA)
  slot(a, 2)
  place(a, support, 7)
  await change(a, target, glass, cursorA)
  await ack(a, 7)

  // Server-authoritative storage: move glass through the main inventory and
  // back. Client changedSlots/cursor reports remain advisory.
  cursorA = a.events.length
  click(a, corrected.stateId, 38, stack(md.itemsByName.glass.id), emptyStack)
  const carrying = await a.expect('window_items', p => p.carriedItem.itemId === md.itemsByName.glass.id && p.items[38].itemCount === 0, cursorA)
  cursorA = a.events.length
  click(a, carrying.stateId, 9, emptyStack, stack(md.itemsByName.glass.id))
  const stored = await a.expect('window_items', p => p.carriedItem.itemCount === 0 && p.items[9].itemId === md.itemsByName.glass.id, cursorA)
  cursorA = a.events.length
  click(a, stored.stateId, 9, stack(md.itemsByName.glass.id), emptyStack)
  const carryingAgain = await a.expect('window_items', p => p.carriedItem.itemId === md.itemsByName.glass.id && p.items[9].itemCount === 0, cursorA)
  cursorA = a.events.length
  click(a, carryingAgain.stateId, 38, emptyStack, stack(md.itemsByName.glass.id))
  const restored = await a.expect('window_items', p => p.carriedItem.itemCount === 0 && p.items[38].itemId === md.itemsByName.glass.id, cursorA)
  assert(restored.stateId > carrying.stateId)
  slot(a, 2)
  cursorA = a.events.length
  place(a, { ...support, x: 12 }, 8)
  await change(a, { ...target, x: 12 }, glass, cursorA)
  await ack(a, 8)

  cursorB = b.events.length
  move(b, 8.5, 8.5)
  const reloaded = chunkWith(await b.expect('map_chunk', p => p.x === 0 && p.z === 0, cursorB))
  assert.equal(reloaded.getBlockStateId(target), glass)
  assert.equal(reloaded.getSkyLight(target), 15)
  assert(!b.events.slice(cursorB).some(([n, p]) => n === 'block_change' && same(p.location, target)), 'out-of-range observer received mutation')

  const mineable = { x: 9, y: 63, z: 8 }
  // Pickaxe mismatches grass' shovel requirement; wooden shovel is sufficient.
  slot(a, 4)
  cursorA = a.events.length
  dig(a, mineable, 9)
  await change(a, mineable, md.blocksByName.grass_block.defaultState, cursorA)
  await ack(a, 9)
  slot(a, 6)
  dig(a, mineable, 10)
  await change(a, mineable, 0, cursorA)
  await ack(a, 10)
  const remote = { x: 15, y: 63, z: 15 }
  cursorA = a.events.length
  dig(a, remote, 11)
  await change(a, remote, md.blocksByName.grass_block.defaultState, cursorA)
  await ack(a, 11)
  cursorA = a.events.length
  place(a, { ...support, x: 11 }, 12, 1)
  await change(a, { ...target, x: 11 }, 0, cursorA)
  await ack(a, 12)
  cursorB = b.events.length
  slot(a, 1)
  place(a, { ...support, x: 11 }, 13)
  await change(b, { ...target, x: 11 }, dirt, cursorB)
  await ack(a, 13)
  console.log('PASS: block/tool categories, tool-kind and mining-level gates, authoritative inventory, placement, sync, light, denial, and reload')
}

const timeout = setTimeout(() => {
  for (const peer of peers) peer.end()
  console.error('Creative block check timed out')
  process.exit(1)
}, 22_000)
run().catch(error => {
  console.error(error)
  process.exitCode = 1
}).finally(() => {
  clearTimeout(timeout)
  for (const peer of peers) peer.end()
})
