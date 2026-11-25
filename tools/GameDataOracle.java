// Runtime block-state facts absent from vanilla data-generator reports.
// Obfuscated symbols are pinned to the SHA-1 verified Java 1.21.1 server JAR.
import java.io.PrintStream;
import java.lang.reflect.*;

class GameDataOracle {
    static Object field(Object owner, String name) throws Exception {
        Class<?> type = owner instanceof Class<?> ? (Class<?>) owner : owner.getClass();
        while (type != null) {
            try {
                Field field = type.getDeclaredField(name);
                field.setAccessible(true);
                return field.get(owner instanceof Class<?> ? null : owner);
            } catch (NoSuchFieldException ignored) {
                type = type.getSuperclass();
            }
        }
        throw new NoSuchFieldException(name);
    }

    public static void main(String[] args) throws Exception {
        Method detect = Class.forName("ab").getDeclaredMethod("a");
        detect.setAccessible(true);
        detect.invoke(null);
        Class<?> bootstrap = Class.forName("akt");
        Method initialize = bootstrap.getDeclaredMethod("a");
        initialize.setAccessible(true);
        initialize.invoke(null);
        PrintStream output = (PrintStream) field(bootstrap, "a");

        Class<?> registries = Class.forName("lt");
        Object blocks = field(registries, "e");
        Object items = field(registries, "g");
        Method blockKey = blocks.getClass().getMethod("b", Object.class);
        Method itemKey = items.getClass().getMethod("b", Object.class);

        Class<?> blockClass = Class.forName("dfy");
        Object states = field(blockClass, "q");
        Method stateId = states.getClass().getMethod("a", Object.class);
        Object emptyWorld = field(Class.forName("dcl"), "a");
        Object zero = field(Class.forName("jd"), "c");
        Class<?> blockGetter = Class.forName("dcc");
        Class<?> blockPos = Class.forName("jd");

        for (Object state : (Iterable<?>) states) {
            int id = (Integer) stateId.invoke(states, state);
            Object block = state.getClass().getMethod("b").invoke(state);
            Object blockName = blockKey.invoke(blocks, block);
            Object item = block.getClass().getMethod("r").invoke(block);
            Object itemName = itemKey.invoke(items, item);
            Object shape = state.getClass().getMethod("k", blockGetter, blockPos)
                    .invoke(state, emptyWorld, zero);
            boolean collision = !(Boolean) shape.getClass().getMethod("c").invoke(shape);
            boolean skylight = (Boolean) state.getClass().getMethod("a", blockGetter, blockPos)
                    .invoke(state, emptyWorld, zero);
            int lightEmission = (Integer) state.getClass().getMethod("h").invoke(state);
            int lightBlock = (Integer) state.getClass().getMethod("b", blockGetter, blockPos)
                    .invoke(state, emptyWorld, zero);
            output.println(id + "\t" + blockName + "\t" + field(state, "n") + "\t"
                    + field(state, "o") + "\t" + field(state, "h") + "\t"
                    + field(state, "j") + "\t" + field(state, "p") + "\t"
                    + field(state, "y") + "\t" + collision + "\t" + skylight + "\t"
                    + lightEmission + "\t" + lightBlock + "\t" + itemName);
        }
    }
}
