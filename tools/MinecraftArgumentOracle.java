// Black-box syntax oracle for the SHA-verified official Java 1.21.1 server.
// Obfuscated class names are pinned by that release's official server mappings.
import com.mojang.brigadier.StringReader;
import com.mojang.brigadier.arguments.ArgumentType;
import com.mojang.brigadier.exceptions.CommandSyntaxException;
import java.nio.charset.StandardCharsets;
import java.util.HexFormat;

class MinecraftArgumentOracle {
    static void run(String kind, String cls, String... inputs) throws Exception {
        Class<?> type = Class.forName(cls);
        var constructor = kind.equals("vec3") || kind.equals("vec2")
            ? type.getDeclaredConstructor(boolean.class) : type.getDeclaredConstructor();
        constructor.setAccessible(true);
        ArgumentType<?> parser = (ArgumentType<?>) (constructor.getParameterCount() == 0
            ? constructor.newInstance() : constructor.newInstance(true));
        for (String input : inputs) {
            StringReader reader = new StringReader(input);
            boolean ok = true;
            try { parser.parse(reader); } catch (CommandSyntaxException error) { ok = false; }
            System.out.println(kind + "\t" + (input.isEmpty() ? "-" : HexFormat.of().formatHex(input.getBytes(StandardCharsets.UTF_8)))
                + "\t" + reader.getCursor() + "\t" + (ok ? "ok" : "error"));
        }
    }
    public static void main(String[] args) throws Exception {
        run("resource", "fu", "stone", "minecraft:stone", ":stone", "a:b/c", "", ":", "Minecraft:stone", "a:b:c", "stone rest");
        run("uuid", "gj", "dd12be42-52a9-4a91-a8a1-11c01849e498", "1-2-3-4-5", "bad", "", "dd12be4252a94a91a8a111c01849e498", "1-2-3-4-5 tail");
        run("int_range", "fr$b", "1", "-2..3", "..5", "5..", "..", "", "3..1", "1.5", "1...5", "1..2 rest");
        run("float_range", "fr$a", "1", ".5", "-2.5..3.5", "..5", "5..", "..", "3..1", "1.2.3", "1e2");
        run("angle", "ez", "0", "~", "~-90", "180", "", "~ 2", "1e2", "^1");
        run("rotation", "gt", "0 0", "~ ~", "~10 -90", "0", "0  0", "^ ^", "1 2 rest");
        run("swizzle", "gu", "xyz", "yx", "xx", "a", "", "xyz rest");
        run("anchor", "ff", "feet", "eyes", "EYES", "", "feet rest");
        run("block_pos", "gp", "0 0 0", "-1 64 2", "~ ~ ~", "~.5 ~1 ~-2", "1.5 2 3", "^ ^ ^", "^1 ^2 ^3", "^1 2 ^3", "~ ~", "0  0 0", "1 2 3 tail");
        run("column_pos", "gq", "0 0", "-1 2", "~ ~", "~.5 ~1", "1.5 2", "^ ^", "1", "1 2 tail");
        run("vec3", "gw", "0 0 0", "-1.5 64 2", "~ ~ ~", "^ ^ ^", "^1 ^2 ^3", "^1 ~2 ^3", "1 2", "0  0 0", "1 2 3 tail");
        run("vec2", "gv", "0 0", "-1.5 2", "~ ~", "^ ^", "1", "1 2 tail");
    }
}
