// Differential oracle for the dependency bundled in Java 1.21.1.
import com.mojang.brigadier.*;
import com.mojang.brigadier.arguments.*;
import com.mojang.brigadier.builder.*;
import com.mojang.brigadier.exceptions.CommandSyntaxException;
import java.nio.charset.StandardCharsets;
import java.util.*;

class BrigadierOracle {
    static LiteralArgumentBuilder<Integer> lit(String name) { return LiteralArgumentBuilder.literal(name); }
    static <T> RequiredArgumentBuilder<Integer,T> arg(String name, ArgumentType<T> type) {
        return RequiredArgumentBuilder.argument(name, type);
    }
    static String hex(String s) { return HexFormat.of().formatHex(s.getBytes(StandardCharsets.UTF_8)); }
    public static void main(String[] args) {
        var d = new CommandDispatcher<Integer>();
        var render = d.register(lit("render").executes(c -> 2)
            .then(lit("on").executes(c -> 3))
            .then(arg("flag", BoolArgumentType.bool()).executes(c -> BoolArgumentType.getBool(c,"flag") ? 11 : 12)));
        d.register(lit("render").then(lit("off").requires(s -> s >= 2).executes(c -> 4)));
        d.register(lit("alias").redirect(render));
        d.register(lit("as").redirect(d.getRoot(), c -> c.getSource()+1));
        d.register(lit("fork").fork(d.getRoot(), c -> List.of(1, 2, 3)));
        d.register(lit("source").executes(c -> c.getSource()));
        d.register(lit("choose")
            .then(arg("num", IntegerArgumentType.integer(-4, 4)).then(lit("end").executes(c -> IntegerArgumentType.getInteger(c,"num"))))
            .then(arg("word", StringArgumentType.word()).executes(c -> StringArgumentType.getString(c,"word").length())));
        d.register(lit("quote").then(arg("text", StringArgumentType.string()).executes(c -> StringArgumentType.getString(c,"text").length())));
        d.register(lit("tail").then(arg("text", StringArgumentType.greedyString()).executes(c -> StringArgumentType.getString(c,"text").length())));
        d.register(lit("long").then(arg("n", LongArgumentType.longArg(-5000000000L, 5000000000L)).executes(c -> (int)(LongArgumentType.getLong(c,"n")/1000000000L))));
        d.register(lit("float").then(arg("n", FloatArgumentType.floatArg(-2.0f, 2.0f)).executes(c -> (int)(10*FloatArgumentType.getFloat(c,"n")))));
        d.register(lit("double").then(arg("n", DoubleArgumentType.doubleArg(-2.0, 2.0)).executes(c -> (int)(10*DoubleArgumentType.getDouble(c,"n")))));
        String[] inputs = {"", "r", "ReN", "render", "render ", "render o", "render on", "render off", "render true", "render 'false'", "render maybe", "render  true", "render\ttrue", "alias", "alias ", "alias on", "alias false", "as source", "as as source", "fork source", "fork as source", "fork nope", "choose 3 end", "choose 3", "choose 7", "choose +3", "choose 3 nope", "choose -4 end", "choose 2147483648", "quote 'hi there'", "quote \"你好🙂\"", "quote \"🙂\"bad", "quote \"🙂\\x\"", "quote \"🙂", "quote ''", "quote '\\''", "quote \"a\\\\b\"", "quote word!", "tail hey  there ", "tail  abc", "tail 🙂 你好", "long 4999999999", "long 5000000001", "long 9223372036854775808", "float .25", "float -.5", "float 2.5", "float 1e0", "double -2.0", "double 1.5", "double -.", " render", "/render"};
        for (int source : new int[]{0, 2}) {
            for (String input : inputs) {
                var parsed = d.parse(input, source);
                String result;
                try { result = "ok:" + d.execute(parsed); }
                catch (CommandSyntaxException e) { result = "err:" + e.getCursor() + ":" + hex(e.getRawMessage().getString()); }
                var suggestions = d.getCompletionSuggestions(parsed).join().getList();
                var out = new StringJoiner(",");
                for (var s : suggestions) {
                    // Server filters suggestions by requirements (unlike Brigadier's low level API).
                    if (source == 0 && s.getText().equals("off")) continue;
                    out.add(s.getRange().getStart() + ":" + s.getRange().getEnd() + ":" + hex(s.getText()));
                }
                System.out.println("case\t" + source + "\t" + hex(input) + "\t" + parsed.getReader().getCursor() + "\t" + result + "\t" + out);
            }
            for (String usage : d.getAllUsage(d.getRoot(), source, true))
                System.out.println("usage\t" + source + "\t" + hex(usage));
            for (String usage : d.getSmartUsage(d.getRoot(), source).values())
                System.out.println("smart\t" + source + "\t" + hex(usage));
        }
    }
}
